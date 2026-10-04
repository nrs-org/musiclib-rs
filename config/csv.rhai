// csv.rhai — CSV rows for match scripts (RFC 4180 quoting).
//
// A module: `import "csv" as csv;` inside the functions that use it. Import it
// once in init() too, so the module is compiled and cached before decide()
// runs on every scoring thread.
//
//   let f = file_create(path);
//   f.write_line(csv::row(["verdict", "p_same"]));
//   f.write_line(csv::row([v.verdict, 0.97]));   // () becomes an empty field

// One field: quoted when it holds a comma, quote or line break.
fn field(v) {
    let s = if type_of(v) == "()" { "" } else { v.to_string() };
    if s.contains(",") || s.contains("\"") || s.contains("\n") || s.contains("\r") {
        s.replace("\"", "\"\"");
        s = "\"" + s + "\"";
    }
    s
}

// One row (no line break) from an array of values.
fn row(values) {
    let parts = [];
    for v in values { parts.push(field(v)); }
    join(parts, ",")
}
