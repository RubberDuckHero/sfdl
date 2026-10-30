# sfdl

Salesforce Template Based Data Loader.

`sfdl` loads CSV data into Salesforce using a YAML template.

---

# Quick Start

```bash
sfdl -t template.yaml -d data.csv -o my-org
```

Dry run:

```bash
sfdl -t template.yaml -d data.csv -o my-org --dry-run
```

---

# YAML Template Spec

## Root

```yaml
name: string
version: number
description: string

target:
  object: SalesforceObject
  operation: insert | update

options:
  strict_headers: true | false

fields: []
```

---

## Field Types

### 1. Column

```yaml
- target: LastName
  type: column
  source: "Last Name"
  required: true
```

---

### 2. Formula

```yaml
- target: Email
  type: formula
  expr: lower(trim(csv.Email))
```

---

### 3. Reference

```yaml
- target: AccountId
  type: reference
  lookup:
    object: Account
    strategy: first_match
    match_rules:
      - field: AccountNumber
        value: trim(csv["Account Number"])
    on_missing: error
    on_multiple: error
```

---

# Formula Language

### Access CSV

```
csv.Email
csv["Account Name"]
```

### Functions

| Function | Description |
|--------|------------|
| trim(x) | remove whitespace |
| lower(x) | lowercase |
| concat(a,b,...) | join strings |
| eq(a,b) | equality |
| if(cond,a,b) | conditional |
| TODAY | todays date YYYY-MM-DD |
| TODAY + x | date relative to today YYYY-MM-DD |

---

# Error Handling

If ANY rows fail:

- Process exits with error
- Failed rows saved automatically

```
<input>.csv → <input>_ERRORS.csv
```

---

# Example Workflow

```bash
sf org login web --alias my-org

sfdl -t contacts.yaml -d contacts.csv -o my-org --dry-run

sfdl -t contacts.yaml -d contacts.csv -o my-org
```

---

# Troubleshooting

## Missing CSV Column

```
Missing CSV column: Email
```

Fix: header mismatch

---

## Lookup Failures

```
No Account record matched
```

Fix: check data or rules

---

## Bulk Failures

```
Job finished but failed records
```

Check:

```
<input>_ERRORS.csv
```

---

# Help

```bash
sfdl --help
sfdl -V
```

# License

Copyright 2026 RubberDuckHero

Redistribution and use in source and binary forms, with or without modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the following disclaimer in the documentation and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS “AS IS” AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
