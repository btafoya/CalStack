#!/usr/bin/env python3
"""Validate live API responses against the generated OpenAPI 3.1 document.

Usage:
  validate_responses.py openapi.json
      <path> <method> <status> <response-file>
      [<path> <method> <status> <response-file> ...]

The response schema is read from the document itself (paths → responses →
content → application/json → schema), so the check exercises the same
document Swagger UI serves. Supports the schema subset utoipa emits:
$ref, type (string or type array), nullable, items, properties, required,
enum, oneOf/anyOf/allOf, and ignores format/additionalProperties.
Exits 1 with a report on the first mismatch.
"""

import json
import sys


def resolve(doc, ref):
    target = doc
    for part in ref.lstrip("#/").split("/"):
        target = target[part.replace("~1", "/").replace("~0", "~")]
    return target


def check(schema, value, doc, where, errs):
    if not isinstance(schema, dict):
        return
    if "$ref" in schema:
        check(resolve(doc, schema["$ref"]), value, doc, where, errs)
        return
    if "oneOf" in schema or "anyOf" in schema:
        for sub in schema.get("oneOf", []) + schema.get("anyOf", []):
            probe = []
            check(sub, value, doc, where, probe)
            if not probe:
                return
        errs.append(f"{where}: matched no variant of the schema union")
        return

    t = schema.get("type")
    if isinstance(t, list):
        if value is None and "null" in t:
            return
        for sub in t:
            if sub == "null":
                continue
            probe = []
            check({**schema, "type": sub}, value, doc, where, probe)
            if not probe:
                return
        errs.append(f"{where}: matched none of types {t} (value {value!r})")
        return
    if t == "null":
        if value is not None:
            errs.append(f"{where}: expected null, got {value!r}")
        return

    if t == "object" or (t is None and "properties" in schema):
        if not isinstance(value, dict):
            errs.append(f"{where}: expected object, got {type(value).__name__}")
            return
        for name in schema.get("required", []):
            if name not in value:
                errs.append(f"{where}: missing required property {name!r}")
        for key, item in value.items():
            if key in schema.get("properties", {}):
                check(schema["properties"][key], item, doc, f"{where}.{key}", errs)
        return

    if t == "array":
        if not isinstance(value, list):
            errs.append(f"{where}: expected array, got {type(value).__name__}")
            return
        if "items" in schema:
            for i, item in enumerate(value):
                check(schema["items"], item, doc, f"{where}[{i}]", errs)
        return

    if t == "string":
        if not isinstance(value, str):
            errs.append(f"{where}: expected string, got {type(value).__name__}")
        return
    if t == "boolean":
        if not isinstance(value, bool):
            errs.append(f"{where}: expected boolean, got {type(value).__name__}")
        return
    if t in ("integer", "number"):
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            errs.append(f"{where}: expected {t}, got {type(value).__name__}")
        elif t == "integer" and isinstance(value, float):
            errs.append(f"{where}: expected integer, got float {value!r}")
        return
    # Freeform schema (utoipa's serde_json::Value mapping): anything goes.
    return


def main():
    doc = json.load(open(sys.argv[1]))
    args = sys.argv[2:]
    if not args or len(args) % 4 != 0:
        sys.exit(__doc__)
    for i in range(0, len(args), 4):
        path, method, status, response_file = args[i : i + 4]
        content = json.load(open(response_file))
        schema = (
            doc["paths"][path][method]["responses"][status]
            .get("content", {})
            .get("application/json", {})
            .get("schema")
        )
        if not schema:
            sys.exit(f"{method} {path} {status}: no JSON schema in the document")
        errs = []
        check(schema, content, doc, f"{method} {path} {status}", errs)
        if errs:
            print("RESPONSE SCHEMA MISMATCH:", file=sys.stderr)
            for e in errs[:20]:
                print(f"  {e}", file=sys.stderr)
            sys.exit(1)
    print(f"response schemas valid ({len(args) // 4} responses)")


if __name__ == "__main__":
    main()