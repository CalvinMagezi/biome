use crate::{JsRuleAction, services::typed::Typed, utils::is_node_equal};
use std::borrow::Cow;

use biome_analyze::{
    FixKind, Rule, RuleDiagnostic, RuleDomain, RuleSource, context::RuleContext, declare_lint_rule,
};
use biome_console::markup;
use biome_js_factory::make;
use biome_js_syntax::{
    AnyJsCallArgument, AnyJsExpression, AnyJsLiteralExpression, JsBinaryExpression,
    JsBinaryOperator, JsCallExpression, JsStaticMemberExpression, JsSyntaxToken, T,
};
use biome_js_type_info::{Literal, ResolvedTypeData, Type, TypeData};
use biome_rowan::{AstNode, AstSeparatedList, BatchMutationExt, TextRange, declare_node_union};
use biome_rule_options::use_string_starts_ends_with::UseStringStartsEndsWithOptions;

declare_lint_rule! {
    /// Prefer `String#startsWith()` and `String#endsWith()` over verbose prefix and suffix checks.
    ///
    /// This rule detects common string comparisons such as indexing, `charAt`, `indexOf`, `lastIndexOf`,
    /// `slice`, `substring`, `match`, and anchored `RegExp#test` calls when they are being used to check
    /// whether a string starts or ends with another string.
    ///
    /// The rule uses type information and only reports when the receiver is known to be a string. Array
    /// indexing and other non-string receivers are ignored.
    ///
    /// ## Examples
    ///
    /// ### Invalid
    ///
    /// ```ts,expect_diagnostic,file=invalid-index.ts
    /// declare const text: string;
    /// text[0] === "a";
    /// ```
    ///
    /// ```ts,expect_diagnostic,file=invalid-search.ts
    /// declare const text: string;
    /// text.indexOf("foo") === 0;
    /// ```
    ///
    /// ```ts,expect_diagnostic,file=invalid-regex.ts
    /// declare const text: string;
    /// /^foo/.test(text);
    /// ```
    ///
    /// ### Valid
    ///
    /// ```ts,file=valid-string.ts
    /// declare const text: string;
    /// text.startsWith("foo");
    /// text.endsWith("bar");
    /// ```
    ///
    /// ```ts,file=valid-array.ts
    /// declare const list: string[];
    /// list[0] === "a";
    /// ```
    pub UseStringStartsEndsWith {
        version: "next",
        name: "useStringStartsEndsWith",
        language: "js",
        recommended: true,
        sources: &[RuleSource::EslintTypeScript("prefer-string-starts-ends-with").inspired()],
        domains: &[RuleDomain::Types],
        fix_kind: FixKind::Unsafe,
    }
}

declare_node_union! {
    pub AnyStartsEndsWithQuery = JsBinaryExpression | JsCallExpression
}

#[derive(Clone, Debug)]
pub struct RuleState {
    method: PreferredMethod,
    pattern: PatternKind,
    range: TextRange,
    fix: Option<FixPlan>,
}

impl Rule for UseStringStartsEndsWith {
    type Query = Typed<AnyStartsEndsWithQuery>;
    type State = RuleState;
    type Signals = Option<Self::State>;
    type Options = UseStringStartsEndsWithOptions;

    fn run(ctx: &RuleContext<Self>) -> Self::Signals {
        match ctx.query() {
            AnyStartsEndsWithQuery::JsBinaryExpression(binary) => {
                run_binary_expression(ctx, binary)
            }
            AnyStartsEndsWithQuery::JsCallExpression(call) => run_call_expression(ctx, call),
        }
    }

    fn diagnostic(_ctx: &RuleContext<Self>, state: &Self::State) -> Option<RuleDiagnostic> {
        let preferred = match state.method {
            PreferredMethod::StartsWith => "startsWith()",
            PreferredMethod::EndsWith => "endsWith()",
        };

        let description = match state.pattern {
            PatternKind::Index => "index access comparison",
            PatternKind::CharAt => "charAt() comparison",
            PatternKind::IndexOf => "indexOf() comparison",
            PatternKind::LastIndexOf => "lastIndexOf() comparison",
            PatternKind::Match => "match() comparison",
            PatternKind::Slice => "slice() comparison",
            PatternKind::Substring => "substring() comparison",
            PatternKind::RegExpTest => "anchored RegExp#test() call",
        };

        let mut diagnostic = RuleDiagnostic::new(
            rule_category!(),
            state.range,
            markup! {
                "This "<Emphasis>{description}</Emphasis>" looks like you are checking a string's "{match state.method { PreferredMethod::StartsWith => "prefix", PreferredMethod::EndsWith => "suffix" }}"."
            },
        ).note(markup! {
            "A dedicated string boundary check is clearer and improves readability."
        });

        if state.fix.is_none() {
            diagnostic = diagnostic.note(markup! {
                "Consider using "<Emphasis>{preferred}</Emphasis>" instead. Biome did not apply an automatic fix because the replacement could change semantics for this specific expression."
            });
        }

        Some(diagnostic)
    }

    fn action(ctx: &RuleContext<Self>, state: &Self::State) -> Option<JsRuleAction> {
        let fix = state.fix.as_ref()?;
        let replace_node = fix.replace_node().clone();
        let replacement = build_replacement(ctx, fix)?;
        let mut mutation = ctx.root().begin();
        mutation.replace_node(replace_node, replacement);

        let preferred = match state.method {
            PreferredMethod::StartsWith => "startsWith()",
            PreferredMethod::EndsWith => "endsWith()",
        };

        Some(JsRuleAction::new(
            ctx.metadata().action_category(ctx.category(), ctx.group()),
            ctx.metadata().applicability(),
            markup! { "Use "<Emphasis>{preferred}</Emphasis>" instead." }.to_owned(),
            mutation,
        ))
    }
}

#[derive(Clone, Debug)]
enum FixPlan {
    MethodCall {
        replace_node: AnyJsExpression,
        object: AnyJsExpression,
        operator_token: JsSyntaxToken,
        argument: AnyJsExpression,
        method: PreferredMethod,
        negated: bool,
    },
    CallRename {
        replace_node: AnyJsExpression,
        call: JsCallExpression,
        object: AnyJsExpression,
        method: PreferredMethod,
        argument_override: Option<AnyJsExpression>,
        negated: bool,
    },
    LiteralMethodCall {
        replace_node: AnyJsExpression,
        object: AnyJsExpression,
        regex: AnyJsExpression,
        operator_token: JsSyntaxToken,
        method: PreferredMethod,
        negated: bool,
    },
}

#[derive(Clone, Copy, Debug)]
enum PreferredMethod {
    StartsWith,
    EndsWith,
}

#[derive(Clone, Copy, Debug)]
enum PatternKind {
    Index,
    CharAt,
    IndexOf,
    LastIndexOf,
    Match,
    Slice,
    Substring,
    RegExpTest,
}

fn run_binary_expression(
    ctx: &RuleContext<UseStringStartsEndsWith>,
    binary: &JsBinaryExpression,
) -> Option<RuleState> {
    match_index_expression(ctx, binary)
        .or_else(|| match_char_at_expression(ctx, binary))
        .or_else(|| match_index_of_expression(ctx, binary))
        .or_else(|| match_last_index_of_expression(ctx, binary))
        .or_else(|| match_match_expression(ctx, binary))
        .or_else(|| match_slice_or_substring_expression(ctx, binary))
}

fn run_call_expression(
    ctx: &RuleContext<UseStringStartsEndsWith>,
    call: &JsCallExpression,
) -> Option<RuleState> {
    match_regexp_test_call(ctx, call)
}

fn match_index_expression(
    ctx: &RuleContext<UseStringStartsEndsWith>,
    binary: &JsBinaryExpression,
) -> Option<RuleState> {
    let comparison = normalize_binary_expression(binary)?;
    let (member, compared) = match comparison.left.clone().omit_parentheses() {
        AnyJsExpression::JsComputedMemberExpression(member) => (member, comparison.right.clone()),
        _ => return None,
    };

    let object = member.object().ok()?;
    if !ensure_known_string_type(ctx, &object) {
        return None;
    }

    let index = member.member().ok()?;
    let method = if is_zero_number_expression(ctx, &index) {
        PreferredMethod::StartsWith
    } else if is_length_minus_number(&object, &index, 1.0)? {
        PreferredMethod::EndsWith
    } else {
        return None;
    };

    let fix = build_index_or_char_at_fix(
        AnyJsExpression::JsBinaryExpression(binary.clone()),
        &object,
        member
            .optional_chain_token()
            .unwrap_or_else(|| make::token(T![.])),
        &compared,
        method,
        comparison.negated,
    );

    Some(RuleState {
        method,
        pattern: PatternKind::Index,
        range: binary.range(),
        fix,
    })
}

fn match_char_at_expression(
    ctx: &RuleContext<UseStringStartsEndsWith>,
    binary: &JsBinaryExpression,
) -> Option<RuleState> {
    let comparison = normalize_binary_expression(binary)?;
    let binding = comparison.left.clone().omit_parentheses();
    let call = binding.as_js_call_expression()?;

    let (member, object) = string_method_call(call, "charAt", ctx)?;
    let args = call.arguments().ok()?.args();
    let first_arg = args.first()?.ok()?;
    let first = first_arg.as_any_js_expression()?;

    let method = if is_zero_number_expression(ctx, first) {
        PreferredMethod::StartsWith
    } else if is_length_minus_number(&object, first, 1.0)? {
        PreferredMethod::EndsWith
    } else {
        return None;
    };

    let fix = build_index_or_char_at_fix(
        AnyJsExpression::JsBinaryExpression(binary.clone()),
        &object,
        member.operator_token().ok()?,
        &comparison.right,
        method,
        comparison.negated,
    );

    Some(RuleState {
        method,
        pattern: PatternKind::CharAt,
        range: binary.range(),
        fix,
    })
}

fn match_index_of_expression(
    ctx: &RuleContext<UseStringStartsEndsWith>,
    binary: &JsBinaryExpression,
) -> Option<RuleState> {
    let comparison = normalize_binary_expression(binary)?;
    let binding = comparison.left.clone().omit_parentheses();
    let call = binding.as_js_call_expression()?;
    let (_, object) = string_method_call(call, "indexOf", ctx)?;
    if !is_zero_number_expression(ctx, &comparison.right) {
        return None;
    }

    Some(RuleState {
        method: PreferredMethod::StartsWith,
        pattern: PatternKind::IndexOf,
        range: binary.range(),
        fix: Some(FixPlan::CallRename {
            replace_node: AnyJsExpression::JsBinaryExpression(binary.clone()),
            call: call.clone(),
            object,
            method: PreferredMethod::StartsWith,
            argument_override: None,
            negated: comparison.negated,
        }),
    })
}

fn match_last_index_of_expression(
    ctx: &RuleContext<UseStringStartsEndsWith>,
    binary: &JsBinaryExpression,
) -> Option<RuleState> {
    let comparison = normalize_binary_expression(binary)?;
    let binding = comparison.left.clone().omit_parentheses();
    let call = binding.as_js_call_expression()?;
    let (_, object) = string_method_call(call, "lastIndexOf", ctx)?;
    let argument = first_call_argument(call)?;

    if !matches_length_minus_value(&object, &comparison.right, ctx, &argument)? {
        return None;
    }

    Some(RuleState {
        method: PreferredMethod::EndsWith,
        pattern: PatternKind::LastIndexOf,
        range: binary.range(),
        fix: Some(FixPlan::CallRename {
            replace_node: AnyJsExpression::JsBinaryExpression(binary.clone()),
            call: call.clone(),
            object,
            method: PreferredMethod::EndsWith,
            argument_override: None,
            negated: comparison.negated,
        }),
    })
}

fn match_match_expression(
    ctx: &RuleContext<UseStringStartsEndsWith>,
    binary: &JsBinaryExpression,
) -> Option<RuleState> {
    let comparison = normalize_null_binary_expression(binary)?;
    let binding = comparison.left.clone().omit_parentheses();
    let call = binding.as_js_call_expression()?;
    let (_, object) = string_method_call(call, "match", ctx)?;
    let regex = first_call_argument(call)?;
    let method = extract_plain_anchored_regex(ctx, &regex)?;
    Some(RuleState {
        method,
        pattern: PatternKind::Match,
        range: binary.range(),
        fix: Some(FixPlan::LiteralMethodCall {
            replace_node: AnyJsExpression::JsBinaryExpression(binary.clone()),
            object,
            regex,
            operator_token: call
                .callee()
                .ok()?
                .as_js_static_member_expression()?
                .operator_token()
                .ok()?,
            method,
            negated: comparison.negated,
        }),
    })
}

fn match_slice_or_substring_expression(
    ctx: &RuleContext<UseStringStartsEndsWith>,
    binary: &JsBinaryExpression,
) -> Option<RuleState> {
    let comparison = normalize_binary_expression(binary)?;
    let binding = comparison.left.clone().omit_parentheses();
    let call = binding.as_js_call_expression()?;
    let callee = call.callee().ok()?.omit_parentheses();
    let member = callee.as_js_static_member_expression()?;
    let slice_method = slice_method_kind(member)?;

    let object = member.object().ok()?;
    if !ensure_known_string_type(ctx, &object) {
        return None;
    }
    let value = comparison.right;
    if !ensure_known_string_type(ctx, &value) {
        return None;
    }
    let args = call.arguments().ok()?.args();
    let first_arg = args.first()?.ok()?;
    let first = first_arg.as_any_js_expression()?;
    let second = args
        .iter()
        .nth(1)
        .and_then(|arg| arg.ok())
        .and_then(|arg| arg.as_any_js_expression().cloned());

    let method = if matches!(slice_method, SliceMethodKind::Slice) {
        match_slice_pattern(ctx, &object, &value, first, second.as_ref())?
    } else {
        match_substring_pattern(ctx, &object, &value, first, second.as_ref())?
    };

    Some(RuleState {
        method,
        pattern: if matches!(slice_method, SliceMethodKind::Slice) {
            PatternKind::Slice
        } else {
            PatternKind::Substring
        },
        range: binary.range(),
        fix: Some(FixPlan::CallRename {
            replace_node: AnyJsExpression::JsBinaryExpression(binary.clone()),
            call: call.clone(),
            object,
            method,
            argument_override: Some(value.clone()),
            negated: comparison.negated,
        }),
    })
}

fn match_regexp_test_call(
    ctx: &RuleContext<UseStringStartsEndsWith>,
    call: &JsCallExpression,
) -> Option<RuleState> {
    let callee = call.callee().ok()?.omit_parentheses();
    let member = callee.as_js_static_member_expression()?;
    if !is_static_member_named(member, "test")? {
        return None;
    }

    let regex = member.object().ok()?;
    let method = extract_plain_anchored_regex(ctx, &regex)?;
    let argument = first_call_argument(call)?;
    if !ensure_known_string_type(ctx, &argument) {
        return None;
    }

    Some(RuleState {
        method,
        pattern: PatternKind::RegExpTest,
        range: call.range(),
        fix: Some(FixPlan::LiteralMethodCall {
            replace_node: AnyJsExpression::JsCallExpression(call.clone()),
            object: argument,
            regex,
            operator_token: member.operator_token().ok()?,
            method,
            negated: false,
        }),
    })
}

#[derive(Clone)]
struct BinaryComparison {
    left: AnyJsExpression,
    right: AnyJsExpression,
    negated: bool,
}

#[derive(Clone, Copy)]
enum SliceMethodKind {
    Slice,
    Substring,
}

impl FixPlan {
    fn replace_node(&self) -> &AnyJsExpression {
        match self {
            Self::MethodCall { replace_node, .. }
            | Self::CallRename { replace_node, .. }
            | Self::LiteralMethodCall { replace_node, .. } => replace_node,
        }
    }
}

/// Builds the final replacement expression only when the analyzer decides to apply the fix.
///
/// ```ts
/// text.indexOf(needle) === 0;
/// // becomes
/// text.startsWith(needle);
/// ```
fn build_replacement(
    ctx: &RuleContext<UseStringStartsEndsWith>,
    fix: &FixPlan,
) -> Option<AnyJsExpression> {
    match fix {
        FixPlan::MethodCall {
            object,
            operator_token,
            argument,
            method,
            negated,
            ..
        } => {
            let call =
                build_method_call(object, operator_token.clone(), *method, argument.clone())?;
            maybe_negate(call, *negated)
        }
        FixPlan::CallRename {
            call,
            object,
            method,
            argument_override,
            negated,
            ..
        } => {
            let call = replace_method_call(call, object, *method, argument_override.clone())?;
            maybe_negate(AnyJsExpression::JsCallExpression(call), *negated)
        }
        FixPlan::LiteralMethodCall {
            object,
            regex,
            operator_token,
            method,
            negated,
            ..
        } => build_method_call_with_regex_literal(
            ctx,
            object,
            regex,
            operator_token.clone(),
            *method,
            *negated,
        ),
    }
}

/// Recognizes `slice` and `substring` without allocating a temporary `String`.
///
/// ```ts
/// text.slice(0, 3) === "foo";
/// text.substring(0, 3) === "foo";
/// ```
fn slice_method_kind(member: &JsStaticMemberExpression) -> Option<SliceMethodKind> {
    let token = member.member().ok()?.as_js_name()?.value_token().ok()?;
    match token.text_trimmed() {
        "slice" => Some(SliceMethodKind::Slice),
        "substring" => Some(SliceMethodKind::Substring),
        _ => None,
    }
}

/// Compares a static member name against a borrowed string.
///
/// ```ts
/// text.match(/^foo/);
/// /foo$/.test(text);
/// ```
fn is_static_member_named(member: &JsStaticMemberExpression, expected: &str) -> Option<bool> {
    let name = member.member().ok()?;
    let name = name.as_js_name()?;
    let token = name.value_token().ok()?;

    Some(token.text_trimmed() == expected)
}

fn normalize_binary_expression(binary: &JsBinaryExpression) -> Option<BinaryComparison> {
    let operator = binary.operator().ok()?;
    let left = binary.left().ok()?;
    let right = binary.right().ok()?;

    match operator {
        JsBinaryOperator::Equality | JsBinaryOperator::StrictEquality => Some(BinaryComparison {
            left,
            right,
            negated: false,
        }),
        JsBinaryOperator::Inequality | JsBinaryOperator::StrictInequality => {
            Some(BinaryComparison {
                left,
                right,
                negated: true,
            })
        }
        _ => None,
    }
}

fn normalize_null_binary_expression(binary: &JsBinaryExpression) -> Option<BinaryComparison> {
    let comparison = normalize_binary_expression(binary)?;
    if is_null_literal(&comparison.right) {
        return Some(BinaryComparison {
            left: comparison.left,
            right: comparison.right,
            negated: !comparison.negated,
        });
    }
    if is_null_literal(&comparison.left) {
        return Some(BinaryComparison {
            left: comparison.right,
            right: comparison.left,
            negated: !comparison.negated,
        });
    }
    None
}

fn is_null_literal(expression: &AnyJsExpression) -> bool {
    matches!(
        expression.clone().omit_parentheses(),
        AnyJsExpression::AnyJsLiteralExpression(AnyJsLiteralExpression::JsNullLiteralExpression(_))
    )
}

/// Matches a string method call like `text.charAt(0)` or `text.indexOf("a")`.
///
/// ```ts
/// text.charAt(0);
/// text.lastIndexOf(needle);
/// ```
fn string_method_call(
    call: &JsCallExpression,
    method_name: &str,
    ctx: &RuleContext<UseStringStartsEndsWith>,
) -> Option<(JsStaticMemberExpression, AnyJsExpression)> {
    let callee = call.callee().ok()?.omit_parentheses();
    let member = callee.as_js_static_member_expression()?;
    if !is_static_member_named(member, method_name)? {
        return None;
    }

    let object = member.object().ok()?;
    if !ensure_known_string_type(ctx, &object) {
        return None;
    }
    Some((member.clone(), object))
}

fn first_call_argument(call: &JsCallExpression) -> Option<AnyJsExpression> {
    call.arguments()
        .ok()?
        .args()
        .first()?
        .ok()?
        .as_any_js_expression()
        .cloned()
}

/// Ensures all resolved type variants are strings before the rule reports.
///
/// ```ts
/// declare const text: string | undefined;
/// declare const values: string[];
/// ```
fn ensure_known_string_type(
    ctx: &RuleContext<UseStringStartsEndsWith>,
    expression: &AnyJsExpression,
) -> bool {
    all_type_variants_match(&ctx.type_of_expression(expression), |current, _| {
        current.is_string_or_string_literal()
    })
}

/// Walks a possibly-unioned type and requires every concrete branch to satisfy the predicate.
///
/// ```ts
/// declare const text: string | string[];
/// declare const suffix: string;
/// ```
fn all_type_variants_match(ty: &Type, mut predicate: impl FnMut(&Type, &TypeData) -> bool) -> bool {
    let mut saw_variant = false;
    let mut pending = vec![ty.clone()];

    while let Some(current) = pending.pop() {
        if current.is_union() {
            let mut variants = current.flattened_union_variants().peekable();
            if variants.peek().is_none() {
                return false;
            }
            saw_variant = true;
            pending.extend(variants);
            continue;
        }

        let Some(raw) = current.resolved_data().map(ResolvedTypeData::as_raw_data) else {
            return false;
        };

        match raw {
            TypeData::Generic(generic) if generic.constraint.is_known() => {
                let Some(constraint) = current.resolve(&generic.constraint) else {
                    return false;
                };
                pending.push(constraint);
            }
            TypeData::Generic(_) => return false,
            _ if predicate(&current, raw) => saw_variant = true,
            _ => return false,
        }
    }

    saw_variant
}

fn is_zero_number_expression(
    ctx: &RuleContext<UseStringStartsEndsWith>,
    expression: &AnyJsExpression,
) -> bool {
    ctx.type_of_expression(expression).is_number_literal(0.0)
}

fn is_length_minus_number(
    object: &AnyJsExpression,
    expression: &AnyJsExpression,
    value: f64,
) -> Option<bool> {
    let binding = expression.clone().omit_parentheses();
    let binary = binding.as_js_binary_expression()?;

    (binary.operator().ok()? == JsBinaryOperator::Minus).then_some(())?;

    let left_binding = binary.left().ok()?.omit_parentheses();
    let left = left_binding.as_js_static_member_expression()?;
    let left_object = left.object().ok()?;

    let right_literal = binary.right().ok().and_then(|right| {
        let right = right.omit_parentheses();
        right
            .as_any_js_literal_expression()
            .and_then(|literal| literal.as_js_number_literal_expression())
            .and_then(|literal| literal.value_token().ok())
            .and_then(|token| token.text_trimmed().parse::<f64>().ok())
    });

    Some(
        is_static_member_named(left, "length")?
            && is_node_equal(left_object.syntax(), object.syntax())
            && right_literal.is_some_and(|literal| literal == value),
    )
}

/// Stores a lazy fix plan for `text[0] === "a"` and `text.charAt(0) === "a"` forms.
///
/// ```ts
/// text[0] === "a";
/// text.charAt(text.length - 1) === "z";
/// ```
fn build_index_or_char_at_fix(
    replace_node: AnyJsExpression,
    object: &AnyJsExpression,
    operator_token: biome_js_syntax::JsSyntaxToken,
    compared: &AnyJsExpression,
    method: PreferredMethod,
    negated: bool,
) -> Option<FixPlan> {
    if compared_string_utf16_len(compared) != Some(1) {
        return None;
    }

    Some(FixPlan::MethodCall {
        replace_node,
        object: object.clone(),
        operator_token,
        argument: compared.clone(),
        method,
        negated,
    })
}

/// Returns the UTF-16 length of a string literal argument when a fix must preserve `charAt`/index semantics.
///
/// ```ts
/// text[0] === "a";
/// text[0] === "👍";
/// ```
fn compared_string_utf16_len(expression: &AnyJsExpression) -> Option<usize> {
    let expression = expression.clone().omit_parentheses();
    match expression {
        AnyJsExpression::AnyJsLiteralExpression(
            AnyJsLiteralExpression::JsStringLiteralExpression(literal),
        ) => Some(literal.inner_string_text().ok()?.encode_utf16().count()),
        _ => None,
    }
}

/// Applies the original `!==` / `=== null` polarity after building a `startsWith`/`endsWith` call.
///
/// ```ts
/// text.indexOf(needle) !== 0;
/// text.match(/foo$/) === null;
/// ```
fn maybe_negate(expression: AnyJsExpression, negated: bool) -> Option<AnyJsExpression> {
    if !negated {
        return Some(expression);
    }

    let expression = if expression.precedence().ok()? < biome_js_syntax::OperatorPrecedence::Unary {
        make::parenthesized(expression).into()
    } else {
        expression
    };

    Some(make::js_unary_expression(make::token(T![!]), expression).into())
}

fn build_method_call(
    object: &AnyJsExpression,
    operator_token: biome_js_syntax::JsSyntaxToken,
    method: PreferredMethod,
    argument: AnyJsExpression,
) -> Option<AnyJsExpression> {
    let callee = AnyJsExpression::JsStaticMemberExpression(make::js_static_member_expression(
        object.clone().trim_trivia()?,
        operator_token,
        make::js_name(make::ident(method_name(method))).into(),
    ));
    let args = call_arguments([AnyJsCallArgument::AnyJsExpression(argument.trim_trivia()?)])?;
    Some(AnyJsExpression::JsCallExpression(
        make::js_call_expression(callee, args).build(),
    ))
}

fn build_method_call_with_literal(
    object: &AnyJsExpression,
    operator_token: biome_js_syntax::JsSyntaxToken,
    method: PreferredMethod,
    text: &str,
    negated: bool,
) -> Option<AnyJsExpression> {
    let callee = AnyJsExpression::JsStaticMemberExpression(make::js_static_member_expression(
        object.clone().trim_trivia()?,
        operator_token,
        make::js_name(make::ident(method_name(method))).into(),
    ));
    let string = AnyJsExpression::AnyJsLiteralExpression(
        make::js_string_literal_expression(make::js_string_literal(text)).into(),
    );
    let args = call_arguments([AnyJsCallArgument::AnyJsExpression(string)])?;
    let call = AnyJsExpression::JsCallExpression(make::js_call_expression(callee, args).build());
    maybe_negate(call, negated)
}

/// Builds a string-method call from a plain anchored regex while keeping regex decoding borrowed
/// until the replacement is materialized.
///
/// ```ts
/// text.match(/^foo/);
/// /bar$/.test(text);
/// ```
fn build_method_call_with_regex_literal(
    ctx: &RuleContext<UseStringStartsEndsWith>,
    object: &AnyJsExpression,
    regex: &AnyJsExpression,
    operator_token: biome_js_syntax::JsSyntaxToken,
    method: PreferredMethod,
    negated: bool,
) -> Option<AnyJsExpression> {
    let ty = ctx.type_of_expression(regex);
    let raw = ty.resolved_data()?.as_raw_data();
    let regex = match raw {
        TypeData::Literal(literal) => match literal.as_ref() {
            Literal::RegExp(regex) => regex,
            _ => return None,
        },
        _ => return None,
    };

    if !regex.flags.is_empty() {
        return None;
    }

    let pattern = regex.pattern.text();
    let text = match method {
        PreferredMethod::StartsWith => decode_plain_regex_text(pattern.strip_prefix('^')?)?,
        PreferredMethod::EndsWith => decode_plain_regex_text(pattern.strip_suffix('$')?)?,
    };

    build_method_call_with_literal(object, operator_token, method, text.as_ref(), negated)
}

fn replace_method_call(
    call: &JsCallExpression,
    object: &AnyJsExpression,
    method: PreferredMethod,
    argument_override: Option<AnyJsExpression>,
) -> Option<JsCallExpression> {
    let callee = call.callee().ok()?.omit_parentheses();
    let member = callee.as_js_static_member_expression()?;
    let updated_callee =
        AnyJsExpression::JsStaticMemberExpression(make::js_static_member_expression(
            object.clone().trim_trivia()?,
            member.operator_token().ok()?,
            make::js_name(make::ident(method_name(method))).into(),
        ));

    let mut updated_call = call.clone().with_callee(updated_callee);
    if let Some(argument) = argument_override {
        updated_call =
            updated_call.with_arguments(call_arguments([AnyJsCallArgument::AnyJsExpression(
                argument.trim_trivia()?,
            )])?);
    }
    Some(updated_call)
}

fn call_arguments<const N: usize>(
    args: [AnyJsCallArgument; N],
) -> Option<biome_js_syntax::JsCallArguments> {
    let separators = if N > 1 {
        vec![make::token(T![,]); N - 1]
    } else {
        vec![]
    };
    Some(make::js_call_arguments(
        make::token(T!['(']),
        make::js_call_argument_list(args, separators),
        make::token(T![')']),
    ))
}

fn method_name(method: PreferredMethod) -> &'static str {
    match method {
        PreferredMethod::StartsWith => "startsWith",
        PreferredMethod::EndsWith => "endsWith",
    }
}

fn matches_length_minus_value(
    object: &AnyJsExpression,
    expression: &AnyJsExpression,
    ctx: &RuleContext<UseStringStartsEndsWith>,
    value: &AnyJsExpression,
) -> Option<bool> {
    let binding = expression.clone().omit_parentheses();
    let binary = binding.as_js_binary_expression()?;

    (binary.operator().ok()? == JsBinaryOperator::Minus).then_some(())?;

    let left_binding = binary.left().ok()?.omit_parentheses();
    let left = left_binding.as_js_static_member_expression()?;
    let left_object = left.object().ok()?;

    Some(
        is_static_member_named(left, "length")?
            && is_node_equal(left_object.syntax(), object.syntax())
            && binary
                .right()
                .ok()
                .is_some_and(|right| matches_length_expression(ctx, &right, value)),
    )
}

fn matches_length_expression(
    ctx: &RuleContext<UseStringStartsEndsWith>,
    expression: &AnyJsExpression,
    value: &AnyJsExpression,
) -> bool {
    if let Some(expected_len) = compared_string_utf16_len(value) {
        return ctx
            .type_of_expression(expression)
            .is_number_literal(expected_len as f64);
    }

    let binding = expression.clone().omit_parentheses();
    let Some(member) = binding.as_js_static_member_expression() else {
        return false;
    };

    member.object().ok().is_some_and(|object| {
        is_static_member_named(member, "length") == Some(true)
            && ensure_expression_match(&object, value)
    })
}

#[inline(always)]
fn ensure_expression_match(left: &AnyJsExpression, right: &AnyJsExpression) -> bool {
    is_node_equal(left.syntax(), right.syntax())
}

/// Recognizes `slice` prefix and suffix comparisons that can become `startsWith` or `endsWith`.
///
/// ```ts
/// text.slice(0, 3) === "bar";
/// text.slice(-needle.length) === needle;
/// ```
fn match_slice_pattern(
    ctx: &RuleContext<UseStringStartsEndsWith>,
    object: &AnyJsExpression,
    value: &AnyJsExpression,
    first: &AnyJsExpression,
    second: Option<&AnyJsExpression>,
) -> Option<PreferredMethod> {
    if is_zero_number_expression(ctx, first)
        && second.is_some_and(|second| matches_length_expression(ctx, second, value))
    {
        return Some(PreferredMethod::StartsWith);
    }

    if let Some(len) = negative_length_expression(first, value, ctx)
        && len
    {
        return Some(PreferredMethod::EndsWith);
    }

    let binding = first.clone().omit_parentheses();
    let start = binding.as_js_binary_expression()?;
    if start.operator().ok()? != JsBinaryOperator::Minus {
        return None;
    }
    let left = start.left().ok()?;
    let right = start.right().ok()?;
    if matches_length_minus_value(
        object,
        &AnyJsExpression::JsBinaryExpression(start.clone()),
        ctx,
        value,
    ) == Some(true)
        && second.is_none()
    {
        return Some(PreferredMethod::EndsWith);
    }

    if ensure_expression_match(
        &left,
        &AnyJsExpression::JsStaticMemberExpression(length_member(object.clone())),
    ) && matches_length_expression(ctx, &right, value)
    {
        return Some(PreferredMethod::EndsWith);
    }
    None
}

/// Recognizes `substring` prefix and suffix comparisons that can become `startsWith` or `endsWith`.
///
/// ```ts
/// text.substring(0, 3) === "bar";
/// text.substring(text.length - 3, text.length) === "bar";
/// ```
fn match_substring_pattern(
    ctx: &RuleContext<UseStringStartsEndsWith>,
    object: &AnyJsExpression,
    value: &AnyJsExpression,
    first: &AnyJsExpression,
    second: Option<&AnyJsExpression>,
) -> Option<PreferredMethod> {
    if is_zero_number_expression(ctx, first)
        && second.is_some_and(|second| matches_length_expression(ctx, second, value))
    {
        return Some(PreferredMethod::StartsWith);
    }

    let second = second?;
    if !ensure_expression_match(
        second,
        &AnyJsExpression::JsStaticMemberExpression(length_member(object.clone())),
    ) {
        return None;
    }
    if matches_length_minus_value(object, first, ctx, value) == Some(true) {
        return Some(PreferredMethod::EndsWith);
    }
    None
}

fn negative_length_expression(
    expression: &AnyJsExpression,
    value: &AnyJsExpression,
    ctx: &RuleContext<UseStringStartsEndsWith>,
) -> Option<bool> {
    let binding = expression.clone().omit_parentheses();
    let unary = binding.as_js_unary_expression()?;
    if unary.operator_token().ok()?.kind() != biome_js_syntax::JsSyntaxKind::MINUS {
        return None;
    }
    Some(matches_length_expression(
        ctx,
        &unary.argument().ok()?,
        value,
    ))
}

fn length_member(object: AnyJsExpression) -> JsStaticMemberExpression {
    make::js_static_member_expression(
        object,
        make::token(T![.]),
        make::js_name(make::ident("length")).into(),
    )
}

/// Extracts plain `^prefix` and `suffix$` regex literals that map directly to string methods.
///
/// ```ts
/// text.match(/^foo/);
/// /bar$/.test(text);
/// ```
fn extract_plain_anchored_regex(
    ctx: &RuleContext<UseStringStartsEndsWith>,
    expression: &AnyJsExpression,
) -> Option<PreferredMethod> {
    let ty = ctx.type_of_expression(expression);
    let raw = ty.resolved_data()?.as_raw_data();
    let regex = match raw {
        TypeData::Literal(literal) => match literal.as_ref() {
            Literal::RegExp(regex) => regex,
            _ => return None,
        },
        _ => return None,
    };

    if !regex.flags.is_empty() {
        return None;
    }

    let pattern = regex.pattern.text();
    if let Some(body) = pattern.strip_prefix('^') {
        decode_plain_regex_text(body)?;
        return Some(PreferredMethod::StartsWith);
    }
    if let Some(body) = pattern.strip_suffix('$') {
        decode_plain_regex_text(body)?;
        return Some(PreferredMethod::EndsWith);
    }
    None
}

/// Decodes a regex body only when it is a plain literal string without metacharacters.
///
/// ```ts
/// /^foo/
/// /bar$/
/// ```
fn decode_plain_regex_text(pattern: &str) -> Option<Cow<'_, str>> {
    // We try to return a borrowed slice when possible to avoid unnecessary allocations,
    // but escaped characters require building a new string.

    if pattern.is_empty() {
        return Some(Cow::Borrowed(""));
    }

    let mut chars = pattern.char_indices().peekable();
    let mut result = Cow::Borrowed(pattern);
    while let Some((index, ch)) = chars.next() {
        if ch == '\\' {
            let (_, escaped) = chars.next()?;
            if escaped.is_ascii_alphanumeric() {
                return None;
            }

            if matches!(result, Cow::Borrowed(_)) {
                result = Cow::Owned(pattern[..index].to_string());
            }

            result.to_mut().push(escaped);
            continue;
        }

        if matches!(
            ch,
            '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$'
        ) {
            return None;
        }

        if let Cow::Owned(result) = &mut result {
            result.push(ch);
        }
    }

    Some(result)
}
