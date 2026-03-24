use biome_js_syntax::{AnyJsExpression, AnyJsName, JsCallExpression, JsStaticMemberExpression};

fn static_member_has_name(member: &JsStaticMemberExpression, expected: &str) -> bool {
    member
        .member()
        .ok()
        .and_then(|member| match member {
            AnyJsName::JsName(name) => name.value_token().ok(),
            _ => None,
        })
        .is_some_and(|token| token.text_trimmed() == expected)
}

fn expression_is_known_test_root(expr: AnyJsExpression) -> bool {
    match expr.omit_parentheses() {
        AnyJsExpression::JsIdentifierExpression(ident) => ident
            .name()
            .and_then(|name| name.value_token())
            .is_ok_and(|token| matches!(token.text_trimmed(), "test" | "it" | "describe")),
        _ => false,
    }
}

fn expression_is_describe_target(expr: AnyJsExpression) -> bool {
    match expr.omit_parentheses() {
        AnyJsExpression::JsIdentifierExpression(ident) => ident
            .name()
            .and_then(|name| name.value_token())
            .is_ok_and(|token| token.text_trimmed() == "describe"),
        AnyJsExpression::JsStaticMemberExpression(member) => {
            static_member_has_name(&member, "describe")
                && member
                    .object()
                    .ok()
                    .is_some_and(expression_is_known_test_root)
        }
        _ => false,
    }
}

/// Returns `true` if the call expression is a test case call:
/// `it(...)`, `test(...)`, and their variants with modifiers
/// (`.only`, `.skip`, `.each`, `.todo`, `.failing`, etc.).
/// Also covers aliases: `xit`, `xtest`, `fit`, `ftest`.
///
/// Describe blocks are excluded — only leaf test cases are matched.
pub(crate) fn is_unit_test(call: &JsCallExpression) -> bool {
    let Ok(callee) = call.callee() else {
        return false;
    };
    let is_test_pattern = callee.contains_a_test_pattern()
        || matches!(
            callee.omit_parentheses(),
            AnyJsExpression::JsCallExpression(each_call)
                if each_call
                    .callee()
                    .ok()
                    .is_some_and(|callee| callee.contains_a_test_each_pattern())
        );
    if !is_test_pattern {
        return false;
    }
    // Exclude describe blocks — we want only leaf test cases
    !is_describe_call(call)
}

/// Returns `true` if the call expression is a describe block:
/// - Bare call: `describe(...)`, `fdescribe(...)`, `xdescribe(...)`
/// - Member call: `test.describe(...)`, `it.describe(...)`, `describe.each(...)`
///   where the object is a known test root (`test`, `it`, or `describe`).
///
/// Only `JsStaticMemberExpression` callees are considered — computed member
/// expressions like `obj["describe"]()` are not matched.
pub(crate) fn is_describe_call(call: &JsCallExpression) -> bool {
    let Ok(callee) = call.callee() else {
        return false;
    };
    let callee = callee.omit_parentheses();

    match callee {
        // describe(...) / fdescribe(...) / xdescribe(...)
        AnyJsExpression::JsIdentifierExpression(ident) => ident
            .name()
            .and_then(|r| r.value_token())
            .is_ok_and(|tok| matches!(tok.text_trimmed(), "describe" | "fdescribe" | "xdescribe")),

        // test.describe(...) / it.describe(...) / describe.each(...) etc.
        AnyJsExpression::JsStaticMemberExpression(member) => {
            // Accept `*.describe(...)` and `*.describe.each(...)`.
            let member_is_describe = static_member_has_name(&member, "describe")
                || (static_member_has_name(&member, "each")
                    && member
                        .object()
                        .ok()
                        .is_some_and(expression_is_describe_target));

            if !member_is_describe {
                return false;
            }

            member
                .object()
                .ok()
                .is_some_and(expression_is_known_test_root)
        }

        AnyJsExpression::JsCallExpression(call) => call
            .callee()
            .ok()
            .map(|callee| callee.omit_parentheses())
            .and_then(|callee| match callee {
                AnyJsExpression::JsStaticMemberExpression(member) => Some(member),
                _ => None,
            })
            .is_some_and(|member| {
                static_member_has_name(&member, "each")
                    && member
                        .object()
                        .ok()
                        .is_some_and(expression_is_describe_target)
            }),

        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use biome_js_parser::JsParserOptions;
    use biome_js_syntax::{JsCallExpression, JsFileSource};
    use biome_rowan::AstNode;

    use super::{is_describe_call, is_unit_test};

    fn first_call(src: &str) -> JsCallExpression {
        let parse =
            biome_js_parser::parse(src, JsFileSource::js_module(), JsParserOptions::default());
        parse
            .syntax()
            .descendants()
            .find_map(JsCallExpression::cast)
            .expect("no call expression found in snippet")
    }

    #[test]
    fn describe_bare() {
        assert!(is_describe_call(&first_call("describe('suite', () => {})")));
    }

    #[test]
    fn fdescribe_bare() {
        assert!(is_describe_call(&first_call(
            "fdescribe('suite', () => {})"
        )));
    }

    #[test]
    fn xdescribe_bare() {
        assert!(is_describe_call(&first_call(
            "xdescribe('suite', () => {})"
        )));
    }

    #[test]
    fn test_describe_member() {
        assert!(is_describe_call(&first_call(
            "test.describe('suite', () => {})"
        )));
    }

    #[test]
    fn it_describe_member() {
        assert!(is_describe_call(&first_call(
            "it.describe('suite', () => {})"
        )));
    }

    #[test]
    fn describe_describe_member() {
        assert!(is_describe_call(&first_call(
            "describe.describe('suite', () => {})"
        )));
    }

    #[test]
    fn describe_each_member() {
        assert!(is_describe_call(&first_call(
            "describe.each([])('suite', () => {})"
        )));
    }

    #[test]
    fn test_describe_each_member() {
        assert!(is_describe_call(&first_call(
            "test.describe.each([])('suite', () => {})"
        )));
    }

    #[test]
    fn it_call_is_not_describe() {
        assert!(!is_describe_call(&first_call("it('test', () => {})")));
    }

    #[test]
    fn test_call_is_not_describe() {
        assert!(!is_describe_call(&first_call("test('test', () => {})")));
    }

    #[test]
    fn unknown_dot_describe_is_not_describe() {
        // "foo" is not a known test root, so foo.describe() should not match.
        assert!(!is_describe_call(&first_call(
            "foo.describe('suite', () => {})"
        )));
    }

    #[test]
    fn computed_member_describe_is_not_describe() {
        // obj["describe"]() uses a computed member — should not match.
        assert!(!is_describe_call(&first_call(
            r#"obj["describe"]('suite', () => {})"#
        )));
    }

    #[test]
    fn it_is_unit_test() {
        assert!(is_unit_test(&first_call("it('does something', () => {})")));
    }

    #[test]
    fn test_is_unit_test() {
        assert!(is_unit_test(&first_call(
            "test('does something', () => {})"
        )));
    }

    #[test]
    fn xit_is_unit_test() {
        assert!(is_unit_test(&first_call("xit('does something', () => {})")));
    }

    #[test]
    fn xtest_is_unit_test() {
        assert!(is_unit_test(&first_call(
            "xtest('does something', () => {})"
        )));
    }

    #[test]
    fn fit_is_unit_test() {
        assert!(is_unit_test(&first_call("fit('does something', () => {})")));
    }

    #[test]
    fn it_only_is_unit_test() {
        assert!(is_unit_test(&first_call(
            "it.only('does something', () => {})"
        )));
    }

    #[test]
    fn test_skip_is_unit_test() {
        assert!(is_unit_test(&first_call(
            "test.skip('does something', () => {})"
        )));
    }

    #[test]
    fn it_each_is_unit_test() {
        assert!(is_unit_test(&first_call(
            "it.each([])('does something', () => {})"
        )));
    }

    #[test]
    fn test_each_is_unit_test() {
        assert!(is_unit_test(&first_call(
            "test.each([])('does something', () => {})"
        )));
    }

    #[test]
    fn describe_is_not_unit_test() {
        assert!(!is_unit_test(&first_call("describe('suite', () => {})")));
    }

    #[test]
    fn test_describe_is_not_unit_test() {
        assert!(!is_unit_test(&first_call(
            "test.describe('suite', () => {})"
        )));
    }

    #[test]
    fn describe_each_is_not_unit_test() {
        assert!(!is_unit_test(&first_call(
            "describe.each([])('suite', () => {})"
        )));
    }

    #[test]
    fn unrelated_call_is_not_unit_test() {
        assert!(!is_unit_test(&first_call("console.log('hello')")));
    }
}
