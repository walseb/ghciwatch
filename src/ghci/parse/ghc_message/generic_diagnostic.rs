use winnow::ascii::space0;
use winnow::ascii::space1;
use winnow::combinator::opt;
use winnow::PResult;
use winnow::Parser;

use crate::ghci::parse::ghc_message::message_body::parse_message_body;
use crate::ghci::parse::ghc_message::path_colon;
use crate::ghci::parse::ghc_message::position;
use crate::ghci::parse::ghc_message::severity;

use super::GhcDiagnostic;

/// Parse a warning or error like this:
///
/// ```plain
/// NotStockDeriveable.hs:6:12: error: [GHC-00158]
///     • Can't make a derived instance of ‘MyClass MyType’:
///         ‘MyClass’ is not a stock derivable class (Eq, Show, etc.)
///     • In the data declaration for ‘MyType’
///     Suggested fix: Perhaps you intended to use DeriveAnyClass
///   |
/// 6 |   deriving MyClass
///   |            ^^^^^^^
/// ```
pub fn generic_diagnostic(input: &mut &str) -> PResult<GhcDiagnostic> {
    // TODO: Confirm that the input doesn't start with space?
    let path = path_colon.parse_next(input)?;
    // Some whole-module diagnostics (notably import cycles in GHC 9.12) have a source path
    // but no line/column range: `Foo.hs: error: ...`.
    let span = opt(position::parse_position_range)
        .parse_next(input)?
        .unwrap_or_default();
    let _ = space1.parse_next(input)?;
    let severity = severity::parse_severity_colon.parse_next(input)?;
    let _ = space0.parse_next(input)?;
    let message = parse_message_body.parse_next(input)?;

    Ok(GhcDiagnostic {
        severity,
        path: Some(path.to_owned()),
        span,
        message: message.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use indoc::indoc;
    use position::PositionRange;
    use pretty_assertions::assert_eq;
    use severity::Severity;

    #[test]
    fn test_parse_diagnostic_message() {
        assert_eq!(
            generic_diagnostic
                .parse(indoc!(
                    "NotStockDeriveable.hs:6:12: error: [GHC-00158]
                        • Can't make a derived instance of ‘MyClass MyType’:
                            ‘MyClass’ is not a stock derivable class (Eq, Show, etc.)
                        • In the data declaration for ‘MyType’
                        Suggested fix: Perhaps you intended to use DeriveAnyClass
                      |
                    6 |   deriving MyClass
                      |            ^^^^^^^
                    "
                ))
                .unwrap(),
            GhcDiagnostic {
                severity: Severity::Error,
                path: Some("NotStockDeriveable.hs".into()),
                span: PositionRange::new(6, 12, 6, 12),
                message: indoc!(
                    "[GHC-00158]
                        • Can't make a derived instance of ‘MyClass MyType’:
                            ‘MyClass’ is not a stock derivable class (Eq, Show, etc.)
                        • In the data declaration for ‘MyType’
                        Suggested fix: Perhaps you intended to use DeriveAnyClass
                      |
                    6 |   deriving MyClass
                      |            ^^^^^^^
                    "
                )
                .into()
            }
        );

        assert_eq!(
            generic_diagnostic
                .parse(
                    "src/MyLib.hs: error: [GHC-92213]\n    Module graph contains a cycle:\n      module `MyLib' (src/MyLib.hs) imports itself\n",
                )
                .unwrap(),
            GhcDiagnostic {
                severity: Severity::Error,
                path: Some("src/MyLib.hs".into()),
                span: PositionRange::default(),
                message: "[GHC-92213]\n    Module graph contains a cycle:\n      module `MyLib' (src/MyLib.hs) imports itself\n"
                    .into(),
            }
        );

        // Doesn't parse another error message.
        assert!(generic_diagnostic
            .parse(indoc!(
                "NotStockDeriveable.hs:6:12: error: [GHC-00158]
                        • Can't make a derived instance of ‘MyClass MyType’:
                            ‘MyClass’ is not a stock derivable class (Eq, Show, etc.)
                        • In the data declaration for ‘MyType’
                        Suggested fix: Perhaps you intended to use DeriveAnyClass
                      |
                    6 |   deriving MyClass
                      |            ^^^^^^^

                    Error: Uh oh!
                    "
            ))
            .is_err(),);
    }

    #[test]
    fn test_diagnostic_display() {
        assert_eq!(
            GhcDiagnostic {
                severity: Severity::Error,
                path: Some("src/MyModule.hs".into()),
                span: PositionRange::new(4, 11, 4, 11),
                message: [
                    "",
                    "    • Couldn't match type ‘[Char]’ with ‘()’",
                    "      Expected: ()",
                    "        Actual: String",
                    "    • In the expression: \"example\"",
                    "      In an equation for ‘example’: example = \"example\"",
                    "  |",
                    "4 | example = \"example\"",
                    "  |           ^^^^^^^^^",
                    "",
                ]
                .join("\n")
            }
            .to_string(),
            indoc!(
                r#"
                src/MyModule.hs:4:11: error:
                    • Couldn't match type ‘[Char]’ with ‘()’
                      Expected: ()
                        Actual: String
                    • In the expression: "example"
                      In an equation for ‘example’: example = "example"
                  |
                4 | example = "example"
                  |           ^^^^^^^^^
                "#
            )
        );
    }
}
