use crate::services::semantic::Semantic;
use biome_analyze::{
    QueryMatch, Rule, RuleDiagnostic, RuleSource, context::RuleContext, declare_lint_rule,
};
use biome_console::fmt::{Display, Formatter};
use biome_console::markup;
use biome_diagnostics::Severity;
use biome_js_semantic::{
    Binding, Capture, CaptureType, ClosureExtensions, JsDeclarationKind, ReferencesExtensions,
    Scope, SemanticModel,
};
use biome_js_syntax::{
    AnyJsForInitializer, AnyJsVariableDeclaration, JsArrowFunctionExpression, JsCallExpression,
    JsFunctionDeclaration, JsFunctionExpression, JsSyntaxKind, JsSyntaxNode, JsVariableKind,
    binding_ext::AnyJsBindingDeclaration,
};
use biome_rowan::{AstNode, SyntaxNodeCast, TextSize, TokenText, declare_node_union};
use biome_rule_options::no_loop_func::NoLoopFuncOptions;
use rustc_hash::FxHashSet;

declare_node_union! {
    pub AnyLoopFunction = JsFunctionDeclaration | JsFunctionExpression | JsArrowFunctionExpression
}

declare_lint_rule! {
    /// Disallow functions declared inside loops that capture unsafe outer variables.
    ///
    /// Functions created in loops can easily observe values from a later iteration instead of the
    /// iteration where they were created. This rule reports functions that capture outer bindings
    /// which may be reassigned while the loop continues.
    ///
    /// The rule ignores plain immediately invoked function expressions (IIFEs), but still reports
    /// async, generator, and self-referential IIFEs because they can escape the current iteration.
    ///
    /// ## Examples
    ///
    /// ### Invalid
    ///
    /// Using `var` for the iteration variable creates a single binding shared across all iterations, so it's unsafe to capture.
    ///
    /// ```js,expect_diagnostic
    /// for (var i = 0; i < 10; i++) {
    ///     handlers.push(() => i);
    /// }
    /// ```
    ///
    /// ```js,expect_diagnostic
    /// let value = 0;
    /// for (let i = 0; i < 10; i++) {
    ///     queue.push(function () {
    ///         return value;
    ///     });
    ///     value += 1;
    /// }
    /// ```
    ///
    /// ### Valid
    ///
    /// Using `let` or `const` for the iteration variable creates a fresh binding each iteration, so it's safe to capture.
    ///
    /// ```js
    /// for (let i = 0; i < 10; i++) {
    ///     handlers.push(() => i);
    /// }
    /// ```
    ///
    /// ```js
    /// for (var i = 0; i < 10; i++) {
    ///     const current = i;
    ///     queue.push(function() {
    ///         return current;
    ///     });
    /// }
    /// ```
    ///
    pub NoLoopFunc {
        version: "next",
        name: "noLoopFunc",
        language: "js",
        recommended: false,
        severity: Severity::Warning,
        sources: &[
            RuleSource::Eslint("no-loop-func").same(),
            RuleSource::EslintTypeScript("no-loop-func").same(),
        ],
    }
}

#[derive(Debug)]
pub struct UnsafeCapture {
    name: TokenText,
}

impl Rule for NoLoopFunc {
    type Query = Semantic<AnyLoopFunction>;
    type State = Box<[UnsafeCapture]>;
    type Signals = Option<Self::State>;
    type Options = NoLoopFuncOptions;

    fn run(ctx: &RuleContext<Self>) -> Self::Signals {
        let function = ctx.query();
        let model = ctx.model();
        let loop_node = get_containing_loop_node(function.syntax(), model)?;

        if is_skippable_iife(function, model) {
            return None;
        }

        let closure = match function {
            AnyLoopFunction::JsFunctionDeclaration(function) => function.closure(model),
            AnyLoopFunction::JsFunctionExpression(function) => function.closure(model),
            AnyLoopFunction::JsArrowFunctionExpression(function) => function.closure(model),
        };

        let mut seen = FxHashSet::default();
        let mut unsafe_captures = Vec::new();

        for capture in all_captures_in_closure(&closure) {
            if is_safe_capture(&loop_node, &capture, model) {
                continue;
            }

            let name = capture_name(&capture)?;
            if seen.insert(name.clone()) {
                unsafe_captures.push(UnsafeCapture { name });
            }
        }

        (!unsafe_captures.is_empty()).then_some(unsafe_captures.into_boxed_slice())
    }

    fn diagnostic(ctx: &RuleContext<Self>, state: &Self::State) -> Option<RuleDiagnostic> {
        let function = ctx.query();

        Some(
            RuleDiagnostic::new(
                rule_category!(),
                function.range(),
                markup! {
                    "This function declared in a loop contains unsafe references to outer variables."
                },
            )
                .note(markup! {
                    "Loop-created functions often run after the loop continues, but captured outer bindings like "<Emphasis>"var"</Emphasis>" variables are reused instead of copied per iteration."
                })
                .note(markup! {
                    "The following variables were detected: "<Emphasis>{CaptureList(state)}</Emphasis>
                })
                .note(markup! {
                    "Move the function outside the loop, capture only per-iteration bindings, or avoid mutating the captured variable across iterations."
                }),
        )
    }
}

struct CaptureList<'a>(&'a [UnsafeCapture]);

impl Display for CaptureList<'_> {
    fn fmt(&self, fmt: &mut Formatter) -> std::io::Result<()> {
        for (index, capture) in self.0.iter().enumerate() {
            if index > 0 {
                fmt.write_markup(markup! { ", " })?;
            }
            fmt.write_markup(markup! {{capture.name.text()}})?;
        }
        Ok(())
    }
}

/// Returns all captures in a closure, including nested closures that stay within the same
/// enclosing function body.
///
/// ## Examples
///
/// ```js
/// for (var i = 0; i < 5; i++) {
///     queue.push(() => () => i);
/// }
/// ```
fn all_captures_in_closure(
    closure: &biome_js_semantic::Closure,
) -> impl Iterator<Item = Capture> + use<> {
    let range = closure.closure_range();
    closure
        .descendents()
        .flat_map(|closure| closure.all_captures())
        .filter(move |capture| {
            !range.contains(capture.declaration_range().start())
                && range.contains(capture.node().text_trimmed_range().start())
        })
}

/// Returns the innermost loop that semantically contains `node`.
///
/// The `for` initializer and the `for..in`/`for..of` right-hand side are outside the loop body,
/// matching ESLint's `no-loop-func` behavior.
///
/// ## Examples
///
/// ```js
/// for (var i = 0; i < items.length; i++) {
///     queue.push(() => i);
/// }
/// ```
fn get_containing_loop_node(node: &JsSyntaxNode, model: &SemanticModel) -> Option<JsSyntaxNode> {
    let mut current = node.clone();

    while let Some(parent) = current.parent() {
        match parent.kind() {
            JsSyntaxKind::JS_WHILE_STATEMENT | JsSyntaxKind::JS_DO_WHILE_STATEMENT => {
                return Some(parent);
            }
            JsSyntaxKind::JS_FOR_STATEMENT => {
                let for_statement = parent.clone().cast::<biome_js_syntax::JsForStatement>()?;
                if for_statement
                    .initializer()
                    .is_none_or(|initializer: AnyJsForInitializer| initializer.syntax() != &current)
                {
                    return Some(parent);
                }
            }
            JsSyntaxKind::JS_FOR_IN_STATEMENT => {
                let for_statement = parent.clone().cast::<biome_js_syntax::JsForInStatement>()?;
                if for_statement.expression().ok()?.syntax() != &current {
                    return Some(parent);
                }
            }
            JsSyntaxKind::JS_FOR_OF_STATEMENT => {
                let for_statement = parent.clone().cast::<biome_js_syntax::JsForOfStatement>()?;
                if for_statement.expression().ok()?.syntax() != &current {
                    return Some(parent);
                }
            }
            kind if is_function_boundary(kind) => {
                if is_skippable_iife_syntax(&parent, model) {
                    current = parent;
                    continue;
                }
                return None;
            }
            _ => {}
        }

        current = parent;
    }

    None
}

/// Returns the outermost loop that should be considered when checking writes to a captured
/// binding.
///
/// ## Examples
///
/// ```js
/// for (let i = 0; i < items.length; i++) {
///     queue.push(() => i);
/// }
/// ```
fn get_top_loop_node(
    loop_node: &JsSyntaxNode,
    excluded_node: Option<&JsSyntaxNode>,
    model: &SemanticModel,
) -> JsSyntaxNode {
    let border = excluded_node.map_or(TextSize::from(0), |node| node.text_range().end());
    let mut top_loop = loop_node.clone();
    let mut current_loop = loop_node.clone();

    while current_loop.text_range().start() >= border {
        top_loop = current_loop.clone();
        let Some(containing_loop) = get_containing_loop_node(&current_loop, model) else {
            break;
        };
        current_loop = containing_loop;
    }

    top_loop
}

/// Returns `true` when a closure capture is safe for `noLoopFunc`.
///
/// Safe captures include type-only references, constants, `using` bindings, and loop-local `let`
/// declarations that create a fresh binding each iteration.
///
/// ## Examples
///
/// ```js
/// for (let i = 0; i < 10; i++) {
///     handlers.push(() => i);
/// }
/// ```
fn is_safe_capture(loop_node: &JsSyntaxNode, capture: &Capture, model: &SemanticModel) -> bool {
    if matches!(capture.ty(), CaptureType::Type) {
        return true;
    }

    let binding = capture.binding();
    if !binding.declaration_kind().declares_value() {
        return true;
    }

    if is_constant_binding(&binding) {
        return true;
    }

    let declaration = binding_declaration(&binding);
    if declaration.as_ref().is_some_and(|decl| {
        decl.is_let()
            && decl.range().start() > loop_node.text_range().start()
            && decl.range().end() < loop_node.text_range().end()
    }) {
        return true;
    }

    let binding_variable_scope = variable_scope(binding.scope());
    let border = get_top_loop_node(
        loop_node,
        declaration
            .as_ref()
            .filter(|decl| decl.is_let())
            .map(|decl| decl.syntax()),
        model,
    )
    .text_range()
    .start();

    binding.all_references().all(|reference| {
        !reference.is_write()
            || (variable_scope(reference.scope()) == binding_variable_scope
                && reference.range_start() < border)
    })
}

/// Returns the variable declaration that introduced a binding, if it comes from a variable-like
/// declaration.
///
/// ## Examples
///
/// ```js
/// const { current } = item;
/// ```
fn binding_declaration(binding: &Binding) -> Option<AnyJsVariableDeclaration> {
    let declaration = binding.tree().declaration()?;
    let declaration = declaration
        .parent_binding_pattern_declaration()
        .unwrap_or(declaration);
    let AnyJsBindingDeclaration::JsVariableDeclarator(declarator) = declaration else {
        return None;
    };

    declarator
        .syntax()
        .ancestors()
        .find_map(AnyJsVariableDeclaration::cast)
}

/// Returns `true` when a binding is declared as `const` or `using`.
///
/// ## Examples
///
/// ```js
/// using resource = open();
/// ```
fn is_constant_binding(binding: &Binding) -> bool {
    match binding.declaration_kind() {
        JsDeclarationKind::Using => true,
        JsDeclarationKind::Value => binding_declaration(binding)
            .is_some_and(|declaration| declaration.variable_kind() == Ok(JsVariableKind::Const)),
        _ => false,
    }
}

/// Returns the display name used in diagnostics for a capture.
///
/// ## Examples
///
/// ```js
/// for (var value = 0; value < 10; value++) {
///     queue.push(() => value);
/// }
/// ```
fn capture_name(capture: &Capture) -> Option<TokenText> {
    capture
        .binding()
        .tree()
        .name_token()
        .ok()
        .map(|token| token.token_text_trimmed())
}

/// Returns the closest variable scope for a binding or reference.
///
/// ## Examples
///
/// ```js
/// function outer() {
///     for (var i = 0; i < 10; i++) {
///         queue.push(() => i);
///     }
/// }
/// ```
fn variable_scope(scope: Scope) -> Scope {
    scope
        .ancestors()
        .find(|scope| scope.is_global_scope() || scope.closure().is_some())
        .unwrap_or(scope)
}

/// Returns `true` when a function is a non-escaping IIFE that `noLoopFunc` should ignore.
///
/// ## Examples
///
/// ```js
/// for (var i = 0; i < 10; i++) {
///     (() => i)();
/// }
/// ```
fn is_skippable_iife(function: &AnyLoopFunction, model: &SemanticModel) -> bool {
    if is_async_or_generator(function) || !is_iife(function.syntax()) {
        return false;
    }

    match function {
        AnyLoopFunction::JsFunctionExpression(function) => {
            !is_self_referential_function_expression(function, model)
        }
        AnyLoopFunction::JsArrowFunctionExpression(_) => true,
        AnyLoopFunction::JsFunctionDeclaration(_) => false,
    }
}

/// Returns `true` when a syntax node is an IIFE that should be ignored while walking ancestor
/// functions.
///
/// ## Examples
///
/// ```js
/// for (var i = 0; i < 10; i++) {
///     (() => {
///         return () => i;
///     })();
/// }
/// ```
fn is_skippable_iife_syntax(node: &JsSyntaxNode, model: &SemanticModel) -> bool {
    if let Some(function) = node.clone().cast::<JsFunctionExpression>() {
        return is_skippable_iife(&AnyLoopFunction::JsFunctionExpression(function), model);
    }

    if let Some(function) = node.clone().cast::<JsArrowFunctionExpression>() {
        return is_skippable_iife(&AnyLoopFunction::JsArrowFunctionExpression(function), model);
    }

    false
}

/// Returns `true` if a named function expression references its own name.
///
/// ## Examples
///
/// ```js
/// (function fun() {
///     queue.push(fun);
/// })();
/// ```
fn is_self_referential_function_expression(
    function: &JsFunctionExpression,
    model: &SemanticModel,
) -> bool {
    function
        .id()
        .and_then(|binding| binding.as_js_identifier_binding().cloned())
        .is_some_and(|binding| binding.all_references(model).next().is_some())
}

/// Returns `true` if `node` is immediately invoked, skipping through parentheses.
///
/// ## Examples
///
/// ```js
/// (() => value)();
/// ```
fn is_iife(node: &JsSyntaxNode) -> bool {
    let mut current = node.clone();

    while let Some(parent) = current.parent() {
        if parent.kind() == JsSyntaxKind::JS_PARENTHESIZED_EXPRESSION {
            current = parent;
            continue;
        }

        if let Some(call) = parent.clone().cast::<JsCallExpression>() {
            return call
                .callee()
                .ok()
                .is_some_and(|callee| callee.syntax() == &current);
        }

        return false;
    }

    false
}

/// Returns `true` for async or generator functions, which are not exempted as plain IIFEs.
///
/// ## Examples
///
/// ```js
/// (async () => value)();
/// ```
fn is_async_or_generator(function: &AnyLoopFunction) -> bool {
    match function {
        AnyLoopFunction::JsFunctionDeclaration(function) => {
            function.async_token().is_some() || function.star_token().is_some()
        }
        AnyLoopFunction::JsFunctionExpression(function) => {
            function.async_token().is_some() || function.star_token().is_some()
        }
        AnyLoopFunction::JsArrowFunctionExpression(function) => function.async_token().is_some(),
    }
}

/// Returns `true` if a syntax kind creates a function boundary for loop lookup.
///
/// ## Examples
///
/// ```js
/// for (var i = 0; i < 10; i++) {
///     function outer() {
///         return () => i;
///     }
/// }
/// ```
fn is_function_boundary(kind: JsSyntaxKind) -> bool {
    matches!(
        kind,
        JsSyntaxKind::JS_FUNCTION_DECLARATION
            | JsSyntaxKind::JS_FUNCTION_EXPRESSION
            | JsSyntaxKind::JS_ARROW_FUNCTION_EXPRESSION
            | JsSyntaxKind::JS_CONSTRUCTOR_CLASS_MEMBER
            | JsSyntaxKind::JS_METHOD_CLASS_MEMBER
            | JsSyntaxKind::JS_GETTER_CLASS_MEMBER
            | JsSyntaxKind::JS_SETTER_CLASS_MEMBER
            | JsSyntaxKind::JS_METHOD_OBJECT_MEMBER
            | JsSyntaxKind::JS_GETTER_OBJECT_MEMBER
            | JsSyntaxKind::JS_SETTER_OBJECT_MEMBER
    )
}
