use calamine::{open_workbook_auto, Data, Reader};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::process::{Command, Stdio};
use tempfile::NamedTempFile;

// CLI

#[derive(Debug, Parser)]
#[command(
    name = "sfdl",
    version,
    about = "Salesforce Template Based Data Loader",
    long_about = r#"
Load CSV data into Salesforce using a YAML template.

Flow:
  1. Read template
  2. Read CSV
  3. Resolve lookups
  4. Apply formulas
  5. Confirm before loading
  6. Load via Salesforce CLI

Examples:
  sfdl --template contact.yaml --data contacts.csv --org my-org
  sfdl -t contact.yaml -d contacts.csv -o my-org --dry-run
  sfdl -t contact.yaml -d contacts.csv -o my-org --dry-run --output preview.csv
"#
)]
struct Cli {
    #[arg(long, short = 't', help = "Path to template YAML file")]
    template: String,

    #[arg(long, short = 'd', help = "Path to data file")]
    data: String,

    #[arg(long, short = 's', help = "Worksheet name when reading Excel files")]
    sheet: Option<String>,

    #[arg(long, short = 'o', help = "Salesforce org alias or username")]
    org: String,

    #[arg(
        long,
        help = "Preview only (skip loading into Salesforce)",
        default_value_t = false
    )]
    dry_run: bool,

    #[arg(
        long,
        help = "Save dry-run output CSV to this path, only works with --dry-run"
    )]
    output: Option<String>,
}

// Template model

#[derive(Debug, Deserialize)]
struct TemplateConfig {
    name: String,
    version: u32,
    description: String,
    target: TemplateTarget,
    options: TemplateOptions,
    fields: Vec<TemplateField>,
}

#[derive(Debug, Deserialize)]
struct TemplateTarget {
    object: String,
    operation: String,
}

#[derive(Debug, Deserialize)]
struct TemplateOptions {
    strict_headers: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum TemplateField {
    #[serde(rename = "column")]
    Column {
        target: String,
        source: String,
        required: Option<bool>,
    },

    #[serde(rename = "formula")]
    Formula { target: String, expr: String },

    #[serde(rename = "reference")]
    Reference {
        target: String,
        lookup: TemplateLookup,
    },
}

#[derive(Debug, Deserialize)]
struct TemplateLookup {
    object: String,
    strategy: String,
    match_rules: Vec<TemplateMatchRule>,
    on_missing: String,
    on_multiple: String,
}

#[derive(Debug, Deserialize)]
struct TemplateMatchRule {
    field: String,
    value: String,
}

// Salesforce model

#[derive(Debug, Deserialize)]
struct SfCliQueryOutput {
    result: SfQueryResult,
}

#[derive(Debug, Deserialize)]
struct SfQueryResult {
    records: Vec<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct SfBulkOutput {
    result: SfBulkResult,
}

#[derive(Debug, Deserialize)]
struct SfBulkResult {
    #[serde(default)]
    id: Option<String>,

    #[serde(default, rename = "jobId")]
    job_id: Option<String>,

    #[serde(default, rename = "numberRecordsProcessed")]
    number_records_processed: Option<u64>,

    #[serde(default, rename = "numberRecordsFailed")]
    number_records_failed: Option<u64>,

    #[serde(default, rename = "processedRecords")]
    processed_records: Option<u64>,

    #[serde(default, rename = "failedRecords")]
    failed_records: Option<u64>,

    #[serde(default, rename = "successfulResults")]
    successful_results: Option<u64>,

    #[serde(default, rename = "failedResults")]
    failed_results: Option<u64>,
}

// Shared types

type CsvRow = HashMap<String, String>;
type OutputObject = HashMap<String, String>;

#[derive(Debug, Clone)]
enum FormulaValue {
    String(String),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LookupCacheKey {
    object: String,
    field: String,
    value: String,
}

enum LoadDecision {
    Yes,
    No,
}

// Entrypoint

fn main() {
    if let Err(e) = run() {
        eprintln!("\n❌ Error:\n{}\n", e);
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    print_banner();

    let template_config = read_template(&cli.template)?;
    let rows = read_data_file(&cli.data, cli.sheet.as_deref(), &template_config)?;
    let lookup_cache = build_lookup_cache(&template_config, &rows, &cli.org)?;
    let objects = build_objects(&template_config, &rows, &lookup_cache)?;

    print_load_summary(&template_config, objects.len());

    if cli.dry_run {
        println!("Dry run enabled, no data will be loaded.");

        if let Some(output_path) = cli.output.as_deref() {
            save_objects_as_csv(&objects, output_path)?;
            println!("Saved dry-run output to: {}", output_path);
        } else {
            print_objects_as_csv(&objects)?;
        }

        return Ok(());
    }

    match confirm_load(&objects)? {
        LoadDecision::Yes => {
            let failed_output_path = error_output_path(&cli.data);

            load_objects_to_salesforce(&template_config, &objects, &cli.org, &failed_output_path)?;
        }
        LoadDecision::No => {
            println!("Cancelled. No records loaded.");
        }
    }

    Ok(())
}

// Template

fn read_template(path: &str) -> Result<TemplateConfig, Box<dyn std::error::Error>> {
    let template_text = std::fs::read_to_string(path)?;
    let template_config = yaml_serde::from_str(&template_text)?;
    Ok(template_config)
}

// Data Read

fn read_data_file(
    path: &str,
    sheet: Option<&str>,
    template_config: &TemplateConfig,
) -> Result<Vec<CsvRow>, Box<dyn std::error::Error>> {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_lowercase();

    match extension.as_str() {
        "csv" => read_csv(path, template_config),
        "xls" | "xlsx" | "xlsm" | "xlsb" | "ods" => read_excel(path, sheet, template_config),
        other => Err(format!(
            "Unsupported data file extension '{}'. Use .csv .xls .xlsx .xlsm .xlsb or .ods",
            other
        )
        .into()),
    }
}

// Object building

fn build_objects(
    template_config: &TemplateConfig,
    rows: &[CsvRow],
    lookup_cache: &HashMap<LookupCacheKey, Vec<String>>,
) -> Result<Vec<OutputObject>, Box<dyn std::error::Error>> {
    let mut objects = Vec::new();

    for (index, row) in rows.iter().enumerate() {
        let row_number = index + 2;
        let mut object = HashMap::new();

        for field in &template_config.fields {
            match field {
                TemplateField::Column {
                    target,
                    source,
                    required,
                } => {
                    let value = row.get(source).map(|s| s.trim()).unwrap_or("");

                    if required.unwrap_or(false) && value.is_empty() {
                        return Err(format!(
                            "Row {}: missing required value for '{}' from CSV column '{}'",
                            row_number, target, source
                        )
                        .into());
                    }

                    if !value.is_empty() {
                        object.insert(target.clone(), value.to_string());
                    }
                }

                TemplateField::Formula { target, expr } => {
                    let value = eval_formula(expr, row).map_err(|e| {
                        format!("Row {}: formula for '{}': {}", row_number, target, e)
                    })?;

                    object.insert(target.clone(), value);
                }

                TemplateField::Reference { target, lookup } => {
                    let resolved_id =
                        resolve_reference(row, lookup, lookup_cache).map_err(|e| {
                            format!("Row {}: reference for '{}': {}", row_number, target, e)
                        })?;

                    if let Some(id) = resolved_id {
                        object.insert(target.clone(), id);
                    }
                }
            }
        }

        objects.push(object);
    }

    Ok(objects)
}

// CSV

fn read_csv(
    path: &str,
    template_config: &TemplateConfig,
) -> Result<Vec<CsvRow>, Box<dyn std::error::Error>> {
    let mut reader = csv::Reader::from_path(path)?;
    let headers = reader.headers()?.clone();

    if template_config.options.strict_headers {
        for field in &template_config.fields {
            if let TemplateField::Column { source, .. } = field {
                if !headers.iter().any(|h| h == source) {
                    return Err(format!("Missing CSV column: {}", source).into());
                }
            }
        }
    }

    let rows: Result<Vec<CsvRow>, csv::Error> = reader.deserialize().collect();

    Ok(rows?)
}

fn write_objects_to_csv<W: Write>(
    writer: W,
    objects: &[OutputObject],
) -> Result<(), Box<dyn std::error::Error>> {
    let headers = output_headers(objects);
    let mut writer = csv::WriterBuilder::new()
        .terminator(csv::Terminator::CRLF)
        .from_writer(writer);

    writer.write_record(&headers)?;

    for object in objects {
        let row = headers
            .iter()
            .map(|header| object.get(header).map(String::as_str).unwrap_or(""))
            .collect::<Vec<_>>();

        writer.write_record(row)?;
    }

    writer.flush()?;
    Ok(())
}

fn print_objects_as_csv(objects: &[OutputObject]) -> Result<(), Box<dyn std::error::Error>> {
    write_objects_to_csv(io::stdout(), objects)?;
    println!();
    Ok(())
}

fn save_objects_as_csv(
    objects: &[OutputObject],
    path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::fs::File::create(path)?;
    write_objects_to_csv(file, objects)?;
    Ok(())
}

fn output_headers(objects: &[OutputObject]) -> Vec<String> {
    let mut headers: Vec<String> = objects.iter().flat_map(|obj| obj.keys().cloned()).collect();

    headers.sort();
    headers.dedup();

    headers
}

fn error_output_path(input_path: &str) -> String {
    let path = std::path::Path::new(input_path);

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");

    let parent = path.parent().unwrap_or_else(|| std::path::Path::new(""));

    parent
        .join(format!("{}_ERRORS.csv", stem))
        .to_string_lossy()
        .to_string()
}

// Excel

fn read_excel(
    path: &str,
    sheet: Option<&str>,
    template_config: &TemplateConfig,
) -> Result<Vec<CsvRow>, Box<dyn std::error::Error>> {
    let mut workbook = open_workbook_auto(path)?;

    let sheet_name = match sheet {
        Some(name) => name.to_string(),
        None => workbook
            .sheet_names()
            .first()
            .ok_or("Workbook has no sheets")?
            .to_string(),
    };

    let range = workbook.worksheet_range(&sheet_name)?;

    let mut rows_iter = range.rows();

    let header_row = rows_iter
        .next()
        .ok_or_else(|| format!("Sheet '{}' is empty", sheet_name))?;

    let headers = header_row
        .iter()
        .map(excel_cell_to_string)
        .collect::<Vec<_>>();

    if template_config.options.strict_headers {
        for field in &template_config.fields {
            if let TemplateField::Column { source, .. } = field {
                if !headers.iter().any(|h| h == source) {
                    return Err(format!(
                        "Missing Excel column '{}' in sheet '{}'",
                        source, sheet_name
                    )
                    .into());
                }
            }
        }
    }

    let mut rows = Vec::new();

    for row in rows_iter {
        let mut map = HashMap::new();

        for (index, header) in headers.iter().enumerate() {
            if header.trim().is_empty() {
                continue;
            }

            let value = row.get(index).map(excel_cell_to_string).unwrap_or_default();

            map.insert(header.clone(), value);
        }

        if map.values().any(|value| !value.trim().is_empty()) {
            rows.push(map);
        }
    }

    Ok(rows)
}

fn excel_cell_to_string(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(s) => s.trim().to_string(),
        Data::Float(n) => {
            if n.fract() == 0.0 {
                format!("{:.0}", n)
            } else {
                n.to_string()
            }
        }
        Data::Int(n) => n.to_string(),
        Data::Bool(b) => b.to_string(),
        Data::Error(e) => format!("{:?}", e),
        Data::DateTime(dt) => dt.to_string(),
        Data::DateTimeIso(s) => s.clone(),
        Data::DurationIso(s) => s.clone(),
    }
}

// Lookup handling

fn build_lookup_cache(
    template_config: &TemplateConfig,
    rows: &[CsvRow],
    org: &str,
) -> Result<HashMap<LookupCacheKey, Vec<String>>, Box<dyn std::error::Error>> {
    let keys = collect_lookup_values(template_config, rows)?;
    let grouped = group_lookup_keys(keys);

    let mut cache: HashMap<LookupCacheKey, Vec<String>> = HashMap::new();

    const BATCH_SIZE: usize = 100;

    for ((object, field), values) in grouped {
        for batch in values.chunks(BATCH_SIZE) {
            let spinner = start_spinner(&format!(
                "Querying {}.{} ({} values)",
                object,
                field,
                batch.len()
            ));

            let in_list = batch
                .iter()
                .map(|v| soql_string(v))
                .collect::<Vec<_>>()
                .join(", ");

            let soql = format!("SELECT Id, {field} FROM {object} WHERE {field} IN ({in_list})");
            let result = call_salesforce_query(org, &soql);

            match result {
                Ok(result) => {
                    spinner.finish_with_message(format!(
                        "✔ {}.{} ({} values)",
                        object,
                        field,
                        batch.len()
                    ));

                    for record in result.result.records {
                        let Some(id) = record.get("Id").and_then(|v| v.as_str()) else {
                            continue;
                        };

                        let Some(match_value) = record.get(&field).and_then(|v| v.as_str()) else {
                            continue;
                        };

                        let key = LookupCacheKey {
                            object: object.clone(),
                            field: field.clone(),
                            value: match_value.to_string(),
                        };

                        cache.entry(key).or_default().push(id.to_string());
                    }
                }
                Err(e) => {
                    spinner.finish_and_clear();
                    return Err(e);
                }
            }
        }
    }

    Ok(cache)
}

fn collect_lookup_values(
    template_config: &TemplateConfig,
    rows: &[CsvRow],
) -> Result<HashSet<LookupCacheKey>, Box<dyn std::error::Error>> {
    let mut keys = HashSet::new();

    for (index, row) in rows.iter().enumerate() {
        let row_number = index + 2;

        for field in &template_config.fields {
            let TemplateField::Reference { lookup, .. } = field else {
                continue;
            };

            for rule in &lookup.match_rules {
                let value = eval_formula(&rule.value, row)
                    .map_err(|e| format!("Row {}: lookup formula error: {}", row_number, e))?;

                let value = value.trim();

                if value.is_empty() {
                    continue;
                }

                keys.insert(LookupCacheKey {
                    object: lookup.object.clone(),
                    field: rule.field.clone(),
                    value: value.to_string(),
                });
            }
        }
    }

    Ok(keys)
}

fn group_lookup_keys(keys: HashSet<LookupCacheKey>) -> HashMap<(String, String), Vec<String>> {
    let mut grouped: HashMap<(String, String), Vec<String>> = HashMap::new();

    for key in keys {
        grouped
            .entry((key.object, key.field))
            .or_default()
            .push(key.value);
    }

    grouped
}

fn resolve_reference(
    row: &CsvRow,
    lookup: &TemplateLookup,
    lookup_cache: &HashMap<LookupCacheKey, Vec<String>>,
) -> Result<Option<String>, String> {
    for rule in &lookup.match_rules {
        let value = eval_formula(&rule.value, row)?;
        let value = value.trim();

        if value.is_empty() {
            continue;
        }

        let key = LookupCacheKey {
            object: lookup.object.clone(),
            field: rule.field.clone(),
            value: value.to_string(),
        };

        let matches = lookup_cache.get(&key).cloned().unwrap_or_default();

        match matches.len() {
            0 => continue,
            1 => return Ok(Some(matches[0].clone())),

            _ => {
                if lookup.on_multiple == "error" {
                    return Err(format!(
                        "Multiple {} records matched {} = '{}'",
                        lookup.object, rule.field, value
                    ));
                }

                if lookup.strategy == "first_match" {
                    return Ok(Some(matches[0].clone()));
                }

                return Err(format!(
                    "Multiple matches found but unsupported strategy '{}'",
                    lookup.strategy
                ));
            }
        }
    }

    if lookup.on_missing == "error" {
        return Err(format!(
            "No {} record matched any lookup rule",
            lookup.object
        ));
    }

    Ok(None)
}

// Salesforce CLI

fn sf_command() -> Command {
    if cfg!(windows) {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg("sf");
        cmd
    } else {
        Command::new("sf")
    }
}

fn call_salesforce_query(
    org: &str,
    soql: &str,
) -> Result<SfCliQueryOutput, Box<dyn std::error::Error>> {
    let output = sf_command()
        .arg("data")
        .arg("query")
        .arg("--target-org")
        .arg(org)
        .arg("--query")
        .arg(soql)
        .arg("--json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    if !output.status.success() {
        return Err(format!(
            "Salesforce CLI query failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    let stdout = String::from_utf8(output.stdout)?;
    let parsed: SfCliQueryOutput = serde_json::from_str(&stdout)?;

    Ok(parsed)
}

fn load_objects_to_salesforce(
    template_config: &TemplateConfig,
    objects: &[OutputObject],
    org: &str,
    failed_output_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if objects.is_empty() {
        println!("No records to load.");
        return Ok(());
    }

    validate_operation_requirements(template_config, objects)?;

    let mut temp_file = NamedTempFile::new()?;
    write_objects_to_csv(temp_file.as_file_mut(), objects)?;

    let csv_path = temp_file.path().to_string_lossy().to_string();

    call_salesforce_bulk_operation(
        &template_config.target.operation,
        org,
        &template_config.target.object,
        &csv_path,
        failed_output_path,
    )?;

    Ok(())
}

fn call_salesforce_bulk_operation(
    operation: &str,
    org: &str,
    object: &str,
    csv_path: &str,
    failed_output_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let spinner = start_spinner(&format!(
        "{} records into {}...",
        match operation {
            "insert" => "Inserting",
            "update" => "Updating",
            _ => "Loading",
        },
        object
    ));

    let command_operation = match operation {
        "insert" => "import",
        "update" => "update",
        other => {
            spinner.finish_and_clear();
            return Err(format!("Unsupported operation '{}'", other).into());
        }
    };

    let output = sf_command()
        .arg("data")
        .arg(command_operation)
        .arg("bulk")
        .arg("--file")
        .arg(csv_path)
        .arg("--sobject")
        .arg(object)
        .arg("--line-ending")
        .arg("CRLF")
        .arg("--wait")
        .arg("10")
        .arg("--target-org")
        .arg(org)
        .arg("--json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        spinner.finish_and_clear();

        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout) {
            let message = json
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("Salesforce bulk operation failed");

            let job_id = json
                .get("data")
                .and_then(|data| data.get("jobId"))
                .and_then(|job_id| job_id.as_str())
                .or_else(|| {
                    json.get("result")
                        .and_then(|result| result.get("id"))
                        .and_then(|id| id.as_str())
                })
                .or_else(|| {
                    json.get("result")
                        .and_then(|result| result.get("jobId"))
                        .and_then(|job_id| job_id.as_str())
                });

            if let Some(job_id) = job_id {
                download_failed_bulk_results(org, job_id, failed_output_path)?;

                return Err(format!(
                    "Salesforce bulk {} failed.\n\n{}\n\nFailed rows saved to: {}",
                    operation, message, failed_output_path
                )
                .into());
            }

            return Err(format!("Salesforce error:\n{}", message).into());
        }

        return Err(format!(
            "Salesforce bulk {} failed.\n\nSTDOUT:\n{}\n\nSTDERR:\n{}",
            operation, stdout, stderr
        )
        .into());
    }

    let bulk_output: SfBulkOutput = serde_json::from_str(&stdout)?;

    let processed = bulk_output
        .result
        .number_records_processed
        .or(bulk_output.result.processed_records)
        .or(bulk_output.result.successful_results)
        .unwrap_or(0);

    let failed = bulk_output
        .result
        .number_records_failed
        .or(bulk_output.result.failed_records)
        .or(bulk_output.result.failed_results)
        .unwrap_or(0);

    if failed > 0 {
        spinner.finish_and_clear();

        let job_id = bulk_output
        .result
        .id
        .as_deref()
        .or(bulk_output.result.job_id.as_deref())
        .ok_or_else(|| {
            format!(
                "Salesforce reported failed rows, but no job id was found in the response.\n\nFull response:\n{}",
                stdout
            )
        })?;

        download_failed_bulk_results(org, job_id, failed_output_path)?;

        return Err(format!(
        "Salesforce bulk {} completed with failed records.\n\nProcessed: {}\nFailed: {}\nFailed rows saved to: {}",
        operation, processed, failed, failed_output_path
    )
    .into());
    }

    spinner.finish_with_message(format!(
        "✔ {} complete for {}. Processed: {}, Failed: {}",
        operation, object, processed, failed
    ));

    Ok(())
}

fn download_failed_bulk_results(
    org: &str,
    job_id: &str,
    output_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let spinner = start_spinner("Downloading failed row results...");

    let endpoint = format!("/services/data/v60.0/jobs/ingest/{}/failedResults", job_id);

    let output = sf_command()
        .arg("org")
        .arg("display")
        .arg("--target-org")
        .arg(org)
        .arg("--json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    if !output.status.success() {
        spinner.finish_and_clear();
        return Err(format!(
            "Failed to get org auth details.\n\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    let org_json: serde_json::Value = serde_json::from_slice(&output.stdout)?;

    let access_token = org_json["result"]["accessToken"]
        .as_str()
        .ok_or("Could not find accessToken from sf org display")?;

    let instance_url = org_json["result"]["instanceUrl"]
        .as_str()
        .ok_or("Could not find instanceUrl from sf org display")?;

    let url = format!("{}{}", instance_url, endpoint);

    let output = Command::new("curl")
        .arg("-sS")
        .arg("-H")
        .arg(format!("Authorization: Bearer {}", access_token))
        .arg("-H")
        .arg("Accept: text/csv")
        .arg(&url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    spinner.finish_and_clear();

    if !output.status.success() {
        return Err(format!(
            "Failed to download failed results.\n\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    std::fs::write(output_path, &output.stdout)?;

    println!("Failed rows saved to: {}", output_path);

    Ok(())
}

fn soql_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

fn soql_string(value: &str) -> String {
    format!("'{}'", soql_escape(value))
}

// Validation

fn validate_operation_requirements(
    template_config: &TemplateConfig,
    objects: &[OutputObject],
) -> Result<(), Box<dyn std::error::Error>> {
    match template_config.target.operation.as_str() {
        "insert" => Ok(()),

        "update" => {
            for (index, object) in objects.iter().enumerate() {
                if object.get("Id").map(|v| v.trim()).unwrap_or("").is_empty() {
                    return Err(format!(
                        "Row {}: update requires an Id field. Add a reference field with target: Id.",
                        index + 2
                    )
                    .into());
                }
            }

            Ok(())
        }

        other => Err(format!("Unsupported operation '{}'. Use insert or update.", other).into()),
    }
}

// Formula evaluation

impl FormulaValue {
    fn as_string(self) -> String {
        match self {
            FormulaValue::String(s) => s,
            FormulaValue::Bool(b) => b.to_string(),
        }
    }

    fn expect_string(self) -> Result<String, String> {
        match self {
            FormulaValue::String(s) => Ok(s),
            FormulaValue::Bool(_) => Err("expected string".to_string()),
        }
    }

    fn expect_bool(self) -> Result<bool, String> {
        match self {
            FormulaValue::Bool(b) => Ok(b),
            FormulaValue::String(_) => Err("expected bool".to_string()),
        }
    }
}

fn eval_formula(expr: &str, row: &CsvRow) -> Result<String, String> {
    let trimmed = expr.trim();

    if is_plain_literal(trimmed) {
        return Ok(trimmed.to_string());
    }

    let mut parser = FormulaParser::new(trimmed, row);
    let value = parser.parse_expr()?;

    parser.skip_ws();

    if !parser.is_eof() {
        return Err(format!(
            "unexpected trailing input near '{}'",
            parser.remaining()
        ));
    }

    Ok(value.as_string())
}

fn is_plain_literal(expr: &str) -> bool {
    !expr.contains('(')
        && !expr.starts_with("csv.")
        && !expr.starts_with("csv[")
        && !expr.starts_with('"')
}

struct FormulaParser<'a> {
    input: &'a str,
    pos: usize,
    row: &'a CsvRow,
}

impl<'a> FormulaParser<'a> {
    fn new(input: &'a str, row: &'a CsvRow) -> Self {
        Self { input, pos: 0, row }
    }

    fn parse_expr(&mut self) -> Result<FormulaValue, String> {
        self.skip_ws();

        if self.peek_char() == Some('"') {
            return Ok(FormulaValue::String(self.parse_string()?));
        }

        let ident = self.parse_ident()?;

        if ident == "csv" {
            return Ok(FormulaValue::String(self.parse_csv_access()?));
        }

        self.skip_ws();

        if self.consume_char('(') {
            let args = self.parse_args()?;
            return self.eval_function(&ident, args);
        }

        Ok(FormulaValue::String(ident))
    }

    fn parse_args(&mut self) -> Result<Vec<FormulaValue>, String> {
        let mut args = Vec::new();

        loop {
            self.skip_ws();

            if self.consume_char(')') {
                break;
            }

            args.push(self.parse_expr()?);

            self.skip_ws();

            if self.consume_char(',') {
                continue;
            }

            if self.consume_char(')') {
                break;
            }

            return Err("expected ',' or ')'".to_string());
        }

        Ok(args)
    }

    fn eval_function(&self, name: &str, args: Vec<FormulaValue>) -> Result<FormulaValue, String> {
        match name {
            "trim" => {
                expect_arg_count(name, &args, 1)?;
                Ok(FormulaValue::String(
                    args[0].clone().expect_string()?.trim().to_string(),
                ))
            }

            "lower" => {
                expect_arg_count(name, &args, 1)?;
                Ok(FormulaValue::String(
                    args[0].clone().expect_string()?.to_lowercase(),
                ))
            }

            "concat" => {
                let mut out = String::new();

                for arg in args {
                    out.push_str(&arg.expect_string()?);
                }

                Ok(FormulaValue::String(out))
            }

            "eq" => {
                expect_arg_count(name, &args, 2)?;

                let left = args[0].clone().expect_string()?;
                let right = args[1].clone().expect_string()?;

                Ok(FormulaValue::Bool(left == right))
            }

            "if" => {
                expect_arg_count(name, &args, 3)?;

                let condition = args[0].clone().expect_bool()?;

                if condition {
                    Ok(args[1].clone())
                } else {
                    Ok(args[2].clone())
                }
            }

            _ => Err(format!("unknown function '{}'", name)),
        }
    }

    fn parse_csv_access(&mut self) -> Result<String, String> {
        self.skip_ws();

        let key = if self.consume_char('.') {
            self.parse_ident()?
        } else if self.consume_char('[') {
            self.skip_ws();

            let key = self.parse_string()?;

            self.skip_ws();

            if !self.consume_char(']') {
                return Err("expected ']' after csv[...]".to_string());
            }

            key
        } else {
            return Err("expected csv.Field or csv[\"Field\"]".to_string());
        };

        Ok(self.row.get(&key).cloned().unwrap_or_default())
    }

    fn parse_string(&mut self) -> Result<String, String> {
        if !self.consume_char('"') {
            return Err("expected string".to_string());
        }

        let mut out = String::new();

        while let Some(ch) = self.next_char() {
            match ch {
                '"' => return Ok(out),

                '\\' => {
                    let escaped = self
                        .next_char()
                        .ok_or_else(|| "unterminated escape sequence".to_string())?;

                    match escaped {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        other => out.push(other),
                    }
                }

                other => out.push(other),
            }
        }

        Err("unterminated string".to_string())
    }

    fn parse_ident(&mut self) -> Result<String, String> {
        self.skip_ws();

        let start = self.pos;

        while let Some(ch) = self.peek_char() {
            if ch.is_alphanumeric() || ch == '_' {
                self.next_char();
            } else {
                break;
            }
        }

        if self.pos == start {
            return Err(format!("expected identifier near '{}'", self.remaining()));
        }

        Ok(self.input[start..self.pos].to_string())
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek_char(), Some(ch) if ch.is_whitespace()) {
            self.next_char();
        }
    }

    fn consume_char(&mut self, expected: char) -> bool {
        if self.peek_char() == Some(expected) {
            self.next_char();
            true
        } else {
            false
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn next_char(&mut self) -> Option<char> {
        let ch = self.peek_char()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn remaining(&self) -> &str {
        &self.input[self.pos..]
    }
}

fn expect_arg_count(name: &str, args: &[FormulaValue], expected: usize) -> Result<(), String> {
    if args.len() != expected {
        return Err(format!(
            "{} expects {} argument(s), got {}",
            name,
            expected,
            args.len()
        ));
    }

    Ok(())
}

// UI

fn print_banner() {
    println!("----------------------------------------");
    println!(" Salesforce Data Loader (sfdl)");
    println!("----------------------------------------\n");
}

fn print_load_summary(template_config: &TemplateConfig, record_count: usize) {
    println!("----------------------------------------");
    println!("Template:    {}", template_config.name);
    println!("Version:     {}", template_config.version);
    println!("Description: {}", template_config.description);
    println!("Object:      {}", template_config.target.object);
    println!("Operation:   {}", template_config.target.operation);
    println!("Records:     {}", record_count);
    println!("----------------------------------------");
}

fn confirm_load(objects: &[OutputObject]) -> Result<LoadDecision, Box<dyn std::error::Error>> {
    loop {
        print!(
            "\nReady to load {} record(s). Continue? (y/n): ",
            objects.len()
        );
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        match input.trim().to_lowercase().as_str() {
            "y" | "yes" => return Ok(LoadDecision::Yes),
            "n" | "no" => return Ok(LoadDecision::No),
            _ => println!("Please enter y or n."),
        }
    }
}

fn start_spinner(message: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();

    pb.set_style(
        ProgressStyle::with_template("{spinner} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );

    pb.set_message(message.to_string());
    pb.enable_steady_tick(std::time::Duration::from_millis(100));

    pb
}
