use biome_analyze::{
    Ast, Rule, RuleDiagnostic, RuleSource, context::RuleContext, declare_lint_rule,
};
use biome_console::markup;
use biome_diagnostics::Severity;
use biome_js_syntax::{
    AnyJsClass, AnyJsClassMemberName, AnyJsExpression, AnyJsFunctionBody, ClassMemberName,
    JsArrowFunctionExpression, JsGetterClassMember, JsMethodClassMember, JsPropertyClassMember,
    JsSetterClassMember, JsStaticInitializationBlockClassMember, JsSuperExpression, JsSyntaxNode,
    JsThisExpression, TsAccessibilityModifier,
};
use biome_rowan::{AstNode, TextRange, TokenText, declare_node_union};
use biome_rule_options::use_this_for_class_methods::{
    IgnoreClassesWithImplements, UseThisForClassMethodsOptions,
};

use crate::services::control_flow::AnyJsControlFlowRoot;

declare_lint_rule! {
    /// Enforce that class methods utilize `this`.
    ///
    /// Instance methods usually communicate that their behavior depends on instance state.
    /// When a class member never uses `this`, it can often be made `static` or moved outside
    /// the class to better reflect its intent.
    ///
    /// This rule checks instance methods, getters, setters, and instance field initializers
    /// whose value is an arrow function or function expression. Constructors, static members,
    /// and static blocks are ignored.
    ///
    /// ## Examples
    ///
    /// ### Invalid
    ///
    /// ```js,expect_diagnostic
    /// class A {
    ///     foo() {
    ///         console.log("Hello");
    ///     }
    /// }
    /// ```
    ///
    /// ### Valid
    ///
    /// ```js
    /// class A {
    ///     foo() {
    ///         this.value = "Hello";
    ///     }
    /// }
    /// ```
    ///
    /// ```js
    /// class A {
    ///     static foo() {}
    /// }
    /// ```
    ///
    /// ## Options
    ///
    /// ### `exceptMethods`
    ///
    /// ```json,options
    /// {
    ///     "options": {
    ///         "exceptMethods": ["render", "#serialize"]
    ///     }
    /// }
    /// ```
    ///
    /// ```js,use_options
    /// class Component {
    ///     render() {}
    ///     #serialize() {}
    /// }
    /// ```
    ///
    /// ### `ignoreOverrideMethods`
    ///
    /// ```json,options
    /// {
    ///     "options": {
    ///         "ignoreOverrideMethods": true
    ///     }
    /// }
    /// ```
    ///
    /// ```ts,use_options
    /// abstract class Base {
    ///     abstract method(): void;
    /// }
    ///
    /// class Derived extends Base {
    ///     override method() {}
    /// }
    /// ```
    ///
    /// ### `ignoreClassesWithImplements`
    ///
    /// ```json,options
    /// {
    ///     "options": {
    ///         "ignoreClassesWithImplements": "all"
    ///     }
    /// }
    /// ```
    ///
    /// ```ts,use_options
    /// interface Service {
    ///     run(): void;
    /// }
    ///
    /// class ServiceImpl implements Service {
    ///     run() {}
    /// }
    /// ```
    ///
    /// ```json,options
    /// {
    ///     "options": {
    ///         "ignoreClassesWithImplements": "public-fields"
    ///     }
    /// }
    /// ```
    ///
    /// ```ts,expect_diagnostic,use_options
    /// interface Service {
    ///     run(): void;
    /// }
    ///
    /// class ServiceImpl implements Service {
    ///     run() {}
    ///     protected helper() {}
    /// }
    /// ```
    pub UseThisForClassMethods {
        version: "next",
        name: "useThisForClassMethods",
        language: "js",
        sources: &[RuleSource::Eslint("class-methods-use-this").same()],
        recommended: false,
        severity: Severity::Warning,
    }
}

impl Rule for UseThisForClassMethods {
    type Query = Ast<AnyClassMemberWithBody>;
    type State = MemberState;
    type Signals = Option<Self::State>;
    type Options = UseThisForClassMethodsOptions;

    fn run(ctx: &RuleContext<Self>) -> Self::Signals {
        let member = ctx.query();

        if member.is_static() {
            return None;
        }

        if ctx.options().ignore_override_methods.unwrap_or(false) && member.is_override() {
            return None;
        }

        if should_ignore_member_in_implemented_class(member, ctx.options()) {
            return None;
        }

        let name = member.name().ok()?;

        if !name.is_computed()
            && ctx
                .options()
                .except_methods
                .as_deref()
                .is_some_and(|except_methods| is_excepted_method(&name, except_methods))
        {
            return None;
        }

        if member.uses_lexical_this()? {
            return None;
        }

        Some(MemberState {
            kind: member.kind(),
            name: rendered_member_name(&name)?,
            range: name.range(),
        })
    }

    fn diagnostic(_: &RuleContext<Self>, state: &Self::State) -> Option<RuleDiagnostic> {
        Some(
            RuleDiagnostic::new(
                rule_category!(),
                state.range,
                markup! {
                    "Expected "<Emphasis>"this"</Emphasis>" to be used by "{state.kind}" "<Emphasis>{state.name.prefix}{state.name.name.text()}{state.name.suffix}</Emphasis>"."
                },
            )
            .note(markup! {
                "Adding functions as class members can be a convenient way to organize code related to a class, but when those functions do not use instance state, it can be misleading to readers. Consider whether "<Emphasis>"static"</Emphasis>" or a module-level function would better communicate the intent of this code."
            })
            .note(markup! {
                "If this member does not depend on instance state, make it "<Emphasis>"static"</Emphasis>" or move it outside the class."
            }),
        )
    }
}

/// Returns the token-backed display name for a class member.
///
/// This keeps the diagnostic state allocation-free while preserving prefixes such
/// as `#` and delimiters such as `[` and `]` for computed members.
fn rendered_member_name(name: &AnyJsClassMemberName) -> Option<RenderedMemberName> {
    match name {
        AnyJsClassMemberName::JsComputedMemberName(name) => Some(RenderedMemberName {
            name: name
                .expression()
                .ok()?
                .syntax()
                .first_token()?
                .token_text_trimmed(),
            prefix: "[",
            suffix: "]",
        }),
        AnyJsClassMemberName::JsLiteralMemberName(name) => Some(RenderedMemberName {
            name: name.value().ok()?.token_text_trimmed(),
            prefix: "",
            suffix: "",
        }),
        AnyJsClassMemberName::JsPrivateClassMemberName(name) => Some(RenderedMemberName {
            name: name.id_token().ok()?.token_text_trimmed(),
            prefix: "#",
            suffix: "",
        }),
        AnyJsClassMemberName::JsMetavariable(_) => None,
    }
}

/// Returns `true` when a class member should be ignored because its class implements
/// an interface and the selected option exempts that member.
///
/// ## Examples
///
/// ```ts
/// interface Service {
///     run(): void;
/// }
///
/// class ServiceImpl implements Service {
///     run() {}
/// }
/// ```
///
/// With `ignoreClassesWithImplements: "all"`, `run` is ignored.
///
/// ```ts
/// interface Service {
///     run(): void;
/// }
///
/// class ServiceImpl implements Service {
///     protected runInternal() {}
/// }
/// ```
///
/// With `ignoreClassesWithImplements: "public-fields"`, `runInternal` is still checked.
fn should_ignore_member_in_implemented_class(
    member: &AnyClassMemberWithBody,
    options: &UseThisForClassMethodsOptions,
) -> bool {
    let Some(ignore_mode) = options.ignore_classes_with_implements else {
        return false;
    };

    let Some(class) = member.syntax().ancestors().find_map(AnyJsClass::cast) else {
        return false;
    };

    if class.implements_clause().is_none() {
        return false;
    }

    match ignore_mode {
        IgnoreClassesWithImplements::All => true,
        IgnoreClassesWithImplements::PublicFields => member.is_public(),
    }
}

/// Returns `true` when a member name matches an entry from `exceptMethods`.
///
/// Computed names are handled by the caller and should not be passed here.
///
/// The names above match `exceptMethods: ["render", "#serialize"]`.
fn is_excepted_method(name: &AnyJsClassMemberName, except_methods: &[Box<str>]) -> bool {
    let Some(name) = name.name() else {
        return false;
    };

    match name {
        ClassMemberName::Private(name) => except_methods
            .iter()
            .any(|excepted| excepted.strip_prefix('#') == Some(name.text())),
        ClassMemberName::Public(name) => except_methods
            .iter()
            .any(|excepted| excepted.as_ref() == name.text()),
    }
}

/// Returns `true` if a member body uses the surrounding lexical `this`.
///
/// Nested arrow functions count because they inherit `this`. Nested non-arrow
/// functions, nested classes, and static blocks do not count.
///
/// ## Examples
///
/// ```js
/// class Example {
///     ok() {
///         return (() => this.value)();
///     }
///
///     invalid() {
///         function read() {
///             return this.value;
///         }
///
///         return read;
///     }
/// }
/// ```
fn uses_lexical_this(node: &JsSyntaxNode) -> bool {
    if JsThisExpression::can_cast(node.kind()) || JsSuperExpression::can_cast(node.kind()) {
        return true;
    }

    if AnyJsClass::can_cast(node.kind())
        || JsStaticInitializationBlockClassMember::can_cast(node.kind())
    {
        return false;
    }

    if AnyJsControlFlowRoot::can_cast(node.kind())
        && !JsArrowFunctionExpression::can_cast(node.kind())
    {
        return false;
    }

    node.children().any(|child| uses_lexical_this(&child))
}

/// Captures the member information needed to render the diagnostic after analysis.
#[derive(Debug)]
pub struct MemberState {
    /// Human-readable member kind used in the diagnostic message.
    kind: &'static str,

    /// Token-backed rendered class member name.
    name: RenderedMemberName,

    /// Highlight range for the member name in the source file.
    range: TextRange,
}

/// Token-backed class member name plus any punctuation needed for diagnostics.
#[derive(Debug)]
struct RenderedMemberName {
    /// Token-backed text for the member name itself.
    name: TokenText,

    /// Optional prefix rendered before `name`, such as `#` or `[`.
    prefix: &'static str,

    /// Optional suffix rendered after `name`, such as `]`.
    suffix: &'static str,
}

declare_node_union! {
    pub AnyClassMemberWithBody = JsGetterClassMember | JsMethodClassMember | JsPropertyClassMember | JsSetterClassMember
}

impl AnyClassMemberWithBody {
    fn uses_lexical_this(&self) -> Option<bool> {
        match self {
            Self::JsGetterClassMember(member) => {
                Some(uses_lexical_this(member.body().ok()?.syntax()))
            }
            Self::JsMethodClassMember(member) => {
                Some(uses_lexical_this(member.body().ok()?.syntax()))
            }
            Self::JsSetterClassMember(member) => {
                Some(uses_lexical_this(member.body().ok()?.syntax()))
            }
            Self::JsPropertyClassMember(member) => {
                uses_lexical_this_in_property_initializer(member)
            }
        }
    }

    fn is_override(&self) -> bool {
        match self {
            Self::JsGetterClassMember(member) => member
                .modifiers()
                .into_iter()
                .any(|modifier| modifier.as_ts_override_modifier().is_some()),
            Self::JsMethodClassMember(member) => member
                .modifiers()
                .into_iter()
                .any(|modifier| modifier.as_ts_override_modifier().is_some()),
            Self::JsPropertyClassMember(member) => member
                .modifiers()
                .into_iter()
                .any(|modifier| modifier.as_ts_override_modifier().is_some()),
            Self::JsSetterClassMember(member) => member
                .modifiers()
                .into_iter()
                .any(|modifier| modifier.as_ts_override_modifier().is_some()),
        }
    }

    fn is_public(&self) -> bool {
        if matches!(
            self.name(),
            Ok(AnyJsClassMemberName::JsPrivateClassMemberName(_))
        ) {
            return false;
        }

        match self.accessibility_modifier() {
            None => true,
            Some(modifier) => modifier.is_public(),
        }
    }

    fn is_static(&self) -> bool {
        match self {
            Self::JsGetterClassMember(member) => member
                .modifiers()
                .into_iter()
                .any(|modifier| modifier.as_js_static_modifier().is_some()),
            Self::JsMethodClassMember(member) => member
                .modifiers()
                .into_iter()
                .any(|modifier| modifier.as_js_static_modifier().is_some()),
            Self::JsPropertyClassMember(member) => member
                .modifiers()
                .into_iter()
                .any(|modifier| modifier.as_js_static_modifier().is_some()),
            Self::JsSetterClassMember(member) => member
                .modifiers()
                .into_iter()
                .any(|modifier| modifier.as_js_static_modifier().is_some()),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::JsGetterClassMember(_) => "getter",
            Self::JsMethodClassMember(_) => "class method",
            Self::JsPropertyClassMember(_) => "class field",
            Self::JsSetterClassMember(_) => "setter",
        }
    }

    fn name(&self) -> biome_rowan::SyntaxResult<AnyJsClassMemberName> {
        match self {
            Self::JsGetterClassMember(member) => member.name(),
            Self::JsMethodClassMember(member) => member.name(),
            Self::JsPropertyClassMember(member) => member.name(),
            Self::JsSetterClassMember(member) => member.name(),
        }
    }

    fn accessibility_modifier(&self) -> Option<TsAccessibilityModifier> {
        match self {
            Self::JsGetterClassMember(member) => member
                .modifiers()
                .into_iter()
                .map(AstNode::into_syntax)
                .find_map(TsAccessibilityModifier::cast),
            Self::JsMethodClassMember(member) => member
                .modifiers()
                .into_iter()
                .map(AstNode::into_syntax)
                .find_map(TsAccessibilityModifier::cast),
            Self::JsPropertyClassMember(member) => member
                .modifiers()
                .into_iter()
                .map(AstNode::into_syntax)
                .find_map(TsAccessibilityModifier::cast),
            Self::JsSetterClassMember(member) => member
                .modifiers()
                .into_iter()
                .map(AstNode::into_syntax)
                .find_map(TsAccessibilityModifier::cast),
        }
    }
}

/// Returns `Some(true)` when a property initializer uses `this` from the field's callable value.
///
/// Non-function field initializers are ignored and return `None`.
///
/// ## Examples
///
/// ```js
/// class Example {
///     field = () => this.value;
/// }
/// ```
///
/// The arrow function above is checked and returns `Some(true)`.
fn uses_lexical_this_in_property_initializer(member: &JsPropertyClassMember) -> Option<bool> {
    let initializer = member.value()?.expression().ok()?;
    let root = function_like_body_root(&initializer)?;
    Some(uses_lexical_this(&root))
}

/// Extracts the syntax node that should be inspected for `this` usage in a field initializer.
///
/// ## Examples
///
/// ```js
/// class Example {
///     arrow = () => this.value;
///     functionExpression = function () {
///         return this.value;
///     };
/// }
/// ```
///
/// Both initializers above return the function body or expression that belongs to the field.
fn function_like_body_root(initializer: &AnyJsExpression) -> Option<JsSyntaxNode> {
    if let Some(arrow) = initializer.as_js_arrow_function_expression() {
        return Some(function_body_root(arrow.body().ok()?));
    }

    Some(
        initializer
            .as_js_function_expression()?
            .body()
            .ok()?
            .into_syntax(),
    )
}

/// Converts an arrow-function body into the syntax node that should be searched for `this`.
///
/// ## Examples
///
/// ```js
/// class Example {
///     expressionBody = () => this.value;
///     blockBody = () => {
///         return this.value;
///     };
/// }
/// ```
///
/// This returns either the expression body or the block body syntax node.
fn function_body_root(body: AnyJsFunctionBody) -> JsSyntaxNode {
    match body {
        AnyJsFunctionBody::AnyJsExpression(expression) => expression.into_syntax(),
        AnyJsFunctionBody::JsFunctionBody(body) => body.into_syntax(),
    }
}
