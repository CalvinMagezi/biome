use crate::{JsRuleAction, services::semantic::Semantic};
use biome_analyze::{
    FixKind, Rule, RuleDiagnostic, RuleSource, context::RuleContext, declare_lint_rule,
    options::PreferredQuote,
};
use biome_console::markup;
use biome_js_factory::make::{
    self, js_string_literal_expression, js_string_literal_single_quotes, js_template_chunk,
    js_template_chunk_element,
};
use biome_js_semantic::SemanticModel;
use biome_js_syntax::{
    AnyJsExpression, AnyJsMemberExpression, AnyJsTemplateElement, JsCallExpression,
    JsReferenceIdentifier, JsTemplateExpression, T, binding_ext::AnyJsBindingDeclaration,
    global_identifier, static_value::StaticValue,
};
use biome_rowan::{AstNode, AstNodeList, BatchMutationExt, TextRange};
use biome_rule_options::use_dom_query_selector::UseDomQuerySelectorOptions;

declare_lint_rule! {
    /// Prefer `querySelector()` and `querySelectorAll()` over older DOM query APIs.
    ///
    /// Using the same DOM query APIs consistently makes the code easier to read and easier to
    /// refine later with more specific selectors.
    ///
    /// This rule prefers `querySelector()` over `getElementById()`, and `querySelectorAll()` over
    /// `getElementsByClassName()`, `getElementsByTagName()`, and `getElementsByName()`.
    ///
    /// ## Examples
    ///
    /// ### Invalid
    ///
    /// ```js,expect_diagnostic
    /// document.getElementById("foo");
    /// ```
    ///
    /// ```js,expect_diagnostic
    /// document.getElementsByClassName("foo bar");
    /// ```
    ///
    /// ```js,expect_diagnostic
    /// document.getElementsByTagName("main");
    /// ```
    ///
    /// ### Valid
    ///
    /// ```js
    /// document.querySelector("#foo");
    /// ```
    ///
    /// ```js
    /// document.querySelectorAll(".foo.bar");
    /// ```
    ///
    pub UseDomQuerySelector {
        version: "next",
        name: "useDomQuerySelector",
        language: "js",
        sources: &[RuleSource::EslintUnicorn("prefer-query-selector").inspired()],
        recommended: false,
        fix_kind: FixKind::Unsafe,
    }
}

#[derive(Clone, Copy, Debug)]
enum QueryMethod {
    ElementById,
    ElementsByClassName,
    ElementsByTagName,
    ElementsByName,
}

impl QueryMethod {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "getElementById" => Self::ElementById,
            "getElementsByClassName" => Self::ElementsByClassName,
            "getElementsByTagName" => Self::ElementsByTagName,
            "getElementsByName" => Self::ElementsByName,
            _ => return None,
        })
    }

    fn preferred_name(self) -> &'static str {
        match self {
            Self::ElementById => "querySelector",
            Self::ElementsByClassName | Self::ElementsByTagName | Self::ElementsByName => {
                "querySelectorAll"
            }
        }
    }
}

pub struct RuleState {
    method: QueryMethod,
    range: TextRange,
    fixable: bool,
}

impl Rule for UseDomQuerySelector {
    type Query = Semantic<JsCallExpression>;
    type State = RuleState;
    type Signals = Option<Self::State>;
    type Options = UseDomQuerySelectorOptions;

    fn run(ctx: &RuleContext<Self>) -> Self::Signals {
        let call = ctx.query();

        if call.is_optional() || call.is_optional_chain() {
            return None;
        }

        let callee = call.callee().ok()?.omit_parentheses();
        let member = AnyJsMemberExpression::cast(callee.into_syntax())?;
        if member.is_optional_chain() {
            return None;
        }

        let method_name = member.member_name()?;
        let method = QueryMethod::from_name(method_name.text())?;

        let argument = first_and_only_argument(call)?;
        is_global_document_like(&member.object().ok()?.omit_parentheses(), ctx.model())?;

        Some(RuleState {
            method,
            range: method_name.range(),
            fixable: can_fix_argument(&argument, method),
        })
    }

    fn diagnostic(_ctx: &RuleContext<Self>, state: &Self::State) -> Option<RuleDiagnostic> {
        let mut diag = RuleDiagnostic::new(
            rule_category!(),
            state.range,
            markup! {
                "This DOM query uses an older DOM query API."
            },
        )
        .note(markup! {
            "Using the same DOM query APIs consistently makes DOM lookups easier to read and easier to refine with more specific selectors."
        });
        if !state.fixable {
            diag = diag.note(markup! {
                "Use "<Emphasis>"querySelector()"</Emphasis>" or "<Emphasis>"querySelectorAll()"</Emphasis>" instead."
            });
        }
        Some(diag)
    }

    fn action(ctx: &RuleContext<Self>, state: &Self::State) -> Option<JsRuleAction> {
        if !state.fixable {
            return None;
        }

        let call = ctx.query();
        let callee = call.callee().ok()?.omit_parentheses();
        let member = AnyJsMemberExpression::cast(callee.into_syntax())?;
        let argument = first_and_only_argument(call)?;

        if member.syntax().has_comments_direct()
            || member.syntax().has_comments_descendants()
            || argument.syntax().has_comments_direct()
            || argument.syntax().has_comments_descendants()
        {
            return None;
        }

        let mut mutation = ctx.root().begin();
        replace_method_name(
            &mut mutation,
            &member,
            state.method.preferred_name(),
            ctx.preferred_quote(),
        )?;

        if let Some(replacement_argument) =
            build_replacement_argument(&argument, state.method, ctx.preferred_quote())
        {
            mutation.replace_node(argument, replacement_argument);
        }

        Some(JsRuleAction::new(
            ctx.metadata().action_category(ctx.category(), ctx.group()),
            ctx.metadata().applicability(),
            match state.method {
                QueryMethod::ElementById => {
                    markup! { "Use "<Emphasis>".querySelector()"</Emphasis>" instead." }
                }
                QueryMethod::ElementsByClassName
                | QueryMethod::ElementsByTagName
                | QueryMethod::ElementsByName => {
                    markup! { "Use "<Emphasis>".querySelectorAll()"</Emphasis>" instead." }
                }
            },
            mutation,
        ))
    }
}

fn first_and_only_argument(call: &JsCallExpression) -> Option<AnyJsExpression> {
    let mut args = call.arguments().ok()?.args().into_iter();
    let argument = args.next()?.ok()?.as_any_js_expression()?.clone();
    args.next().is_none().then_some(argument)
}

fn identifier_is_global_document(
    reference: &JsReferenceIdentifier,
    name: &StaticValue,
    model: &SemanticModel,
) -> bool {
    name.text() == "document" && model.binding(reference).is_none()
}

fn is_global_document_like(expr: &AnyJsExpression, model: &SemanticModel) -> Option<()> {
    let (reference, name) = global_identifier(expr)?;

    if identifier_is_global_document(&reference, &name, model) {
        return Some(());
    }

    let binding = model.binding(&reference)?;
    let declaration = binding.tree().declaration()?;
    let declaration = declaration
        .parent_binding_pattern_declaration()
        .unwrap_or(declaration);

    match declaration {
        AnyJsBindingDeclaration::JsVariableDeclarator(declarator) => {
            let initializer = declarator.initializer()?;
            let initializer = initializer.expression().ok()?;
            is_global_document_like(&initializer, model)
        }
        _ => None,
    }
}

fn can_fix_argument(argument: &AnyJsExpression, method: QueryMethod) -> bool {
    if matches!(method, QueryMethod::ElementsByTagName) {
        return true;
    }

    let argument = argument.clone().omit_parentheses();
    match argument {
        AnyJsExpression::AnyJsLiteralExpression(literal) => {
            if literal.as_js_null_literal_expression().is_some() {
                return true;
            }

            literal
                .as_js_string_literal_expression()
                .and_then(|string| string.inner_string_text().ok())
                .is_some_and(|text| !text.text().trim().is_empty())
        }
        AnyJsExpression::JsTemplateExpression(template) => template_is_fixable(&template),
        _ => false,
    }
}

fn template_is_fixable(template: &JsTemplateExpression) -> bool {
    template.tag().is_none()
        && template
            .elements()
            .iter()
            .all(|element| element.as_js_template_chunk_element().is_some())
        && AnyJsExpression::JsTemplateExpression(template.clone())
            .as_static_value()
            .is_some_and(|value| {
                value
                    .as_string_constant()
                    .is_some_and(|text| !text.trim().is_empty())
            })
}

fn build_replacement_argument(
    argument: &AnyJsExpression,
    method: QueryMethod,
    preferred_quote: PreferredQuote,
) -> Option<AnyJsExpression> {
    let argument = argument.clone().omit_parentheses();

    if matches!(method, QueryMethod::ElementsByTagName) {
        return None;
    }

    match argument {
        AnyJsExpression::AnyJsLiteralExpression(literal) => {
            if literal.as_js_null_literal_expression().is_some() {
                return None;
            }

            let string = literal.as_js_string_literal_expression()?;
            let value = string.inner_string_text().ok()?;
            let replacement = match method {
                QueryMethod::ElementById => format!("#{}", value.text()),
                QueryMethod::ElementsByClassName => class_selector(value.text())?,
                QueryMethod::ElementsByName => name_selector(value.text(), preferred_quote)?,
                QueryMethod::ElementsByTagName => return None,
            };

            Some(make_string_literal_expression(
                &replacement,
                preferred_quote,
            ))
        }
        AnyJsExpression::JsTemplateExpression(template) => {
            let static_value =
                AnyJsExpression::JsTemplateExpression(template.clone()).as_static_value()?;
            let value = static_value.as_string_constant()?;
            let replacement = match method {
                QueryMethod::ElementById => format!("#{value}"),
                QueryMethod::ElementsByClassName => class_selector(value)?,
                QueryMethod::ElementsByName => name_selector(value, preferred_quote)?,
                QueryMethod::ElementsByTagName => return None,
            };

            Some(AnyJsExpression::JsTemplateExpression(
                make::js_template_expression(
                    make::token(T!['`']),
                    make::js_template_element_list([AnyJsTemplateElement::from(
                        js_template_chunk_element(js_template_chunk(&replacement)),
                    )]),
                    make::token(T!['`']),
                )
                .build(),
            ))
        }
        _ => None,
    }
}

fn replace_method_name(
    mutation: &mut biome_rowan::BatchMutation<biome_js_syntax::JsLanguage>,
    member: &AnyJsMemberExpression,
    replacement: &str,
    preferred_quote: PreferredQuote,
) -> Option<()> {
    match member {
        AnyJsMemberExpression::JsStaticMemberExpression(static_member) => {
            mutation.replace_element(
                static_member.member().ok()?.into(),
                make::js_name(make::ident(replacement)).into(),
            );
        }
        AnyJsMemberExpression::JsComputedMemberExpression(computed_member) => {
            let current_member = computed_member.member().ok()?;
            let replacement = AnyJsExpression::AnyJsLiteralExpression(
                js_string_literal_expression(if preferred_quote.is_double() {
                    make::js_string_literal(replacement)
                } else {
                    js_string_literal_single_quotes(replacement)
                })
                .into(),
            );
            mutation.replace_node(current_member, replacement);
        }
    }

    Some(())
}

fn class_selector(value: &str) -> Option<String> {
    let mut selector = String::new();
    for class_name in value.split_whitespace() {
        selector.push('.');
        selector.push_str(class_name);
    }
    (!selector.is_empty()).then_some(selector)
}

fn name_selector(value: &str, preferred_quote: PreferredQuote) -> Option<String> {
    if value.is_empty() {
        return None;
    }

    let inner_quote = if preferred_quote.is_double() {
        '\''
    } else {
        '"'
    };
    Some(format!("[name={inner_quote}{value}{inner_quote}]"))
}

fn make_string_literal_expression(value: &str, preferred_quote: PreferredQuote) -> AnyJsExpression {
    AnyJsExpression::AnyJsLiteralExpression(
        js_string_literal_expression(if preferred_quote.is_double() {
            make::js_string_literal(value)
        } else {
            js_string_literal_single_quotes(value)
        })
        .into(),
    )
}
