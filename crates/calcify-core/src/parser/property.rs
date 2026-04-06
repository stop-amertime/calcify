//! Parsing for `@property` declarations.
//!
//! Extracts `syntax`, `inherits`, and `initial-value` descriptors from:
//! ```css
//! @property --name {
//!   syntax: "<integer>";
//!   inherits: false;
//!   initial-value: 0;
//! }
//! ```

use cssparser::Parser;

use crate::error::Result;
use crate::types::*;

/// Parse the body of an `@property` rule given the property name.
///
/// Expects the parser to be positioned at the `{` block contents.
pub fn parse_property_body<'i, 't>(name: &str, input: &mut Parser<'i, 't>) -> Result<PropertyDef> {
    let mut syntax = PropertySyntax::Any;
    let mut inherits = false;
    let mut initial_value = None;

    // Parse descriptor declarations inside the block
    while !input.is_exhausted() {
        let descriptor = input
            .expect_ident_cloned()
            .map_err(|e| crate::CalcifyError::Parse(format!("expected descriptor name: {e:?}")))?;

        input
            .expect_colon()
            .map_err(|e| crate::CalcifyError::Parse(format!("expected ':': {e:?}")))?;

        match &*descriptor {
            "syntax" => {
                let s = input.expect_string_cloned().map_err(|e| {
                    crate::CalcifyError::Parse(format!("expected syntax string: {e:?}"))
                })?;
                syntax = parse_syntax(&s)?;
            }
            "inherits" => {
                let ident = input.expect_ident_cloned().map_err(|e| {
                    crate::CalcifyError::Parse(format!("expected 'true' or 'false': {e:?}"))
                })?;
                inherits = match &*ident {
                    "true" => true,
                    "false" => false,
                    other => {
                        return Err(crate::CalcifyError::Parse(format!(
                            "invalid inherits value: {other}"
                        )));
                    }
                };
            }
            "initial-value" => {
                // Parse the initial value as a number or ident
                initial_value = Some(parse_initial_value(input)?);
            }
            other => {
                // Skip unknown descriptors
                log::warn!("unknown @property descriptor: {other}");
                // Consume tokens until semicolon
                while input.try_parse(|i| i.expect_semicolon()).is_err() {
                    if input.next().is_err() {
                        break;
                    }
                }
                continue;
            }
        }

        // Consume the trailing semicolon (optional for the last descriptor)
        let _ = input.try_parse(|i| i.expect_semicolon());
    }

    Ok(PropertyDef {
        name: name.to_string(),
        syntax,
        inherits,
        initial_value,
    })
}

/// Parse the `syntax` descriptor string (e.g., `"<integer>"`).
fn parse_syntax(s: &str) -> Result<PropertySyntax> {
    match s.trim() {
        "<integer>" => Ok(PropertySyntax::Integer),
        "<number>" => Ok(PropertySyntax::Number),
        "<length>" => Ok(PropertySyntax::Length),
        "*" => Ok(PropertySyntax::Any),
        other => Ok(PropertySyntax::Custom(other.to_string())),
    }
}

/// Parse an initial-value (number, negative number, or ident).
fn parse_initial_value<'i, 't>(input: &mut Parser<'i, 't>) -> Result<CssValue> {
    let state = input.state();
    match input.next().cloned() {
        Ok(cssparser::Token::Number { value, .. }) => Ok(CssValue::Integer(value as i64)),
        Ok(cssparser::Token::Ident(ref s)) => Ok(CssValue::String(s.to_string())),
        Ok(cssparser::Token::QuotedString(ref s)) => Ok(CssValue::String(s.to_string())),
        Ok(_) => {
            input.reset(&state);
            // Try reading remaining text as a string
            let pos = input.position();
            while input.next().is_ok() {}
            let text = input.slice_from(pos).trim();
            Ok(CssValue::String(text.to_string()))
        }
        Err(e) => Err(crate::CalcifyError::Parse(format!(
            "expected initial-value: {e:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cssparser::ParserInput;

    fn parse_prop(name: &str, body: &str) -> Result<PropertyDef> {
        let mut input = ParserInput::new(body);
        let mut parser = Parser::new(&mut input);
        parse_property_body(name, &mut parser)
    }

    #[test]
    fn integer_property() {
        let prop = parse_prop(
            "--AX",
            r#"syntax: "<integer>"; inherits: false; initial-value: 0"#,
        )
        .unwrap();
        assert_eq!(prop.name, "--AX");
        assert_eq!(prop.syntax, PropertySyntax::Integer);
        assert!(!prop.inherits);
        assert!(matches!(prop.initial_value, Some(CssValue::Integer(0))));
    }

    #[test]
    fn any_syntax_property() {
        let prop = parse_prop("--clock", r#"syntax: "*"; inherits: true"#).unwrap();
        assert_eq!(prop.syntax, PropertySyntax::Any);
        assert!(prop.inherits);
        assert!(prop.initial_value.is_none());
    }
}
