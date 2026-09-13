use serde::Deserialize;
use winnow::ascii::space0;
use winnow::combinator::preceded;
use winnow::PResult;
use winnow::Parser;

use crate::ghci::parse::lines::rest_of_line;

use super::GhcDiagnostic;
use super::PositionRange;
use super::Severity;

/// A nullable JSON value which is nevertheless required to be present in the object.
#[derive(Debug, Deserialize)]
#[serde(transparent)]
struct Nullable<T>(Option<T>);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsonDiagnostic {
    #[allow(dead_code)]
    version: String,
    #[allow(dead_code)]
    ghc_version: String,
    span: Nullable<JsonSpan>,
    severity: JsonSeverity,
    #[allow(dead_code)]
    code: Nullable<i64>,
    message: Vec<String>,
    hints: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct JsonSpan {
    file: String,
    start: JsonLocation,
    end: JsonLocation,
}

#[derive(Debug, Deserialize)]
struct JsonLocation {
    line: usize,
    column: usize,
}

#[derive(Debug, Deserialize)]
enum JsonSeverity {
    Warning,
    Error,
}

/// Parse one line emitted by GHC's `-fdiagnostics-as-json` flag.
///
/// Requiring the schema's core fields prevents unrelated application JSON with a `severity` field
/// from being mistaken for a compiler diagnostic. Unknown fields are accepted so newer schema
/// versions remain usable.
pub(crate) fn parse_json_diagnostic_line(line: &str) -> Option<GhcDiagnostic> {
    if !line.trim_start().starts_with('{') {
        return None;
    }

    deserialize_json_diagnostic(line).ok()
}

fn deserialize_json_diagnostic(line: &str) -> serde_json::Result<GhcDiagnostic> {
    let diagnostic: JsonDiagnostic = serde_json::from_str(line)?;
    let severity = match diagnostic.severity {
        JsonSeverity::Warning => Severity::Warning,
        JsonSeverity::Error => Severity::Error,
    };
    let (path, span) = match diagnostic.span.0 {
        Some(span) => (
            Some(span.file.into()),
            PositionRange::new(
                span.start.line,
                span.start.column,
                span.end.line,
                span.end.column,
            ),
        ),
        None => (None, PositionRange::default()),
    };

    let mut message = diagnostic.message.join("\n");
    for hint in diagnostic.hints {
        if !message.is_empty() && !message.ends_with('\n') {
            message.push('\n');
        }
        message.push_str("Suggested fix:\n  ");
        message.push_str(&hint.replace('\n', "\n  "));
    }
    if !message.ends_with('\n') {
        message.push('\n');
    }

    Ok(GhcDiagnostic {
        severity,
        path,
        span,
        message,
    })
}

/// Winnow adapter for the line-oriented mixed GHC output parser.
pub(super) fn json_diagnostic(input: &mut &str) -> PResult<GhcDiagnostic> {
    preceded(space0, ("{", rest_of_line).recognize())
        .try_map(deserialize_json_diagnostic)
        .parse_next(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_error_with_span_and_hints() {
        let diagnostic = parse_json_diagnostic_line(
            r#" {"hints":["Use wantedValue"],"message":["Variable not in scope:","  wanted"],"code":88464,"severity":"Error","span":{"end":{"column":12,"line":4},"file":"src/Foo.hs","start":{"column":3,"line":4}},"ghcVersion":"9.12.2","version":"1.1"}"#,
        )
        .unwrap();

        assert_eq!(diagnostic.severity, Severity::Error);
        assert_eq!(
            diagnostic.path.as_deref(),
            Some(camino::Utf8Path::new("src/Foo.hs"))
        );
        assert_eq!(diagnostic.span, PositionRange::new(4, 3, 4, 12));
        assert_eq!(
            diagnostic.message,
            "Variable not in scope:\n  wanted\nSuggested fix:\n  Use wantedValue\n"
        );
    }

    #[test]
    fn parses_warning_without_location() {
        let diagnostic = parse_json_diagnostic_line(
            r#"{"version":"1.1","ghcVersion":"9.12.2","span":null,"severity":"Warning","code":null,"message":["Warning text"],"hints":[]}"#,
        )
        .unwrap();

        assert_eq!(diagnostic.severity, Severity::Warning);
        assert_eq!(diagnostic.path, None);
        assert_eq!(diagnostic.span, PositionRange::default());
    }

    #[test]
    fn rejects_unrelated_or_incomplete_json() {
        assert!(parse_json_diagnostic_line(r#"{"severity":"Error"}"#).is_none());
        assert!(parse_json_diagnostic_line("not json").is_none());
        assert!(parse_json_diagnostic_line("{malformed}").is_none());
    }
}
