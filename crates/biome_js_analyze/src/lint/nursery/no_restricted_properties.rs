use biome_analyze::{
    Ast, Rule, RuleDiagnostic, RuleSource, context::RuleContext, declare_lint_rule,
};
use biome_console::fmt::{self, Display};
use biome_console::markup;
use biome_diagnostics::Severity;
use biome_js_syntax::{
    AnyJsAssignmentPattern, AnyJsExpression, AnyJsMemberExpression,
    AnyJsObjectAssignmentPatternMember, AnyJsObjectBindingPatternMember, JsAssignmentExpression,
    JsComputedMemberExpression, JsObjectAssignmentPattern, JsObjectBindingPattern,
    JsStaticMemberExpression, JsVariableDeclarator,
};
use biome_rowan::{AstNode, TextRange, TokenText, declare_node_union};
use biome_rule_options::no_restricted_properties::{
    NoRestrictedPropertiesOptions, RestrictedPropertyEntry,
};

declare_lint_rule! {
    /// Disallow specific object properties.
    ///
    /// This rule lets you ban property access for exact object/property pairs, all properties on a
    /// given object, or a property name everywhere except for a short allowlist of objects.
    ///
    /// It also reports restricted properties when they appear in object destructuring.
    ///
    /// ## Examples
    ///
    /// This rule requires explicit configuration to specify which properties are restricted, so it does not report anything by default.
    ///
    /// ## Options
    ///
    /// Use `entries` to define the restricted combinations.
    ///
    /// ### Exact object/property restriction
    ///
    /// ```json,options
    /// {
    ///   "options": {
    ///     "entries": [
    ///       {
    ///         "object": "require",
    ///         "property": "ensure",
    ///         "message": "Use dynamic import() instead."
    ///       }
    ///     ]
    ///   }
    /// }
    /// ```
    ///
    /// #### Invalid
    ///
    /// ```js,use_options,expect_diagnostic
    /// require.ensure("./entry")
    /// ```
    ///
    /// #### Valid
    ///
    /// ```js,use_options
    /// require.resolve("./entry")
    /// ```
    ///
    /// ### Property-wide restriction with an allowlist
    ///
    /// ```json,options
    /// {
    ///   "options": {
    ///     "entries": [
    ///       {
    ///         "property": "__defineGetter__",
    ///         "message": "Use Object.defineProperty() instead.",
    ///         "allowObjects": ["Object"]
    ///       }
    ///     ]
    ///   }
    /// }
    /// ```
    ///
    /// #### Invalid
    ///
    /// ```js,use_options,expect_diagnostic
    /// foo.__defineGetter__
    /// ```
    ///
    /// #### Valid
    ///
    /// ```js,use_options
    /// Object.__defineGetter__
    /// ```
    ///
    /// ### Object-wide restriction with allowed exceptions
    ///
    /// ```json,options
    /// {
    ///   "options": {
    ///     "entries": [
    ///       {
    ///         "object": "arguments",
    ///         "message": "Avoid accessing arbitrary arguments properties.",
    ///         "allowProperties": ["length"]
    ///       }
    ///     ]
    ///   }
    /// }
    /// ```
    ///
    /// #### Invalid
    ///
    /// ```js,use_options,expect_diagnostic
    /// arguments.callee
    /// ```
    ///
    /// #### Valid
    ///
    /// ```js,use_options
    /// arguments.length
    /// ```
    pub NoRestrictedProperties {
        version: "next",
        name: "noRestrictedProperties",
        language: "js",
        sources: &[RuleSource::Eslint("no-restricted-properties").same()],
        recommended: false,
        severity: Severity::Warning,
    }
}

declare_node_union! {
    pub AnyRestrictedPropertyNode = JsStaticMemberExpression | JsComputedMemberExpression | JsObjectBindingPattern | JsObjectAssignmentPattern
}

#[derive(Debug, Clone)]
pub struct RestrictedPropertyState {
    range: TextRange,
    kind: RestrictedPropertyKind,
    entry_index: usize,
}

impl Rule for NoRestrictedProperties {
    type Query = Ast<AnyRestrictedPropertyNode>;
    type State = RestrictedPropertyState;
    type Signals = Box<[Self::State]>;
    type Options = NoRestrictedPropertiesOptions;

    fn run(ctx: &RuleContext<Self>) -> Self::Signals {
        let Some(entries) = ctx.options().entries.as_deref() else {
            return Vec::new().into_boxed_slice();
        };

        match ctx.query() {
            AnyRestrictedPropertyNode::JsStaticMemberExpression(node) => inspect_member_expression(
                &AnyJsMemberExpression::JsStaticMemberExpression(node.clone()),
                entries,
            )
            .into_iter()
            .collect::<Vec<_>>()
            .into_boxed_slice(),
            AnyRestrictedPropertyNode::JsComputedMemberExpression(node) => {
                inspect_member_expression(
                    &AnyJsMemberExpression::JsComputedMemberExpression(node.clone()),
                    entries,
                )
                .into_iter()
                .collect::<Vec<_>>()
                .into_boxed_slice()
            }
            AnyRestrictedPropertyNode::JsObjectBindingPattern(node) => {
                inspect_object_binding_pattern(node, entries)
            }
            AnyRestrictedPropertyNode::JsObjectAssignmentPattern(node) => {
                inspect_object_assignment_pattern(node, entries)
            }
        }
    }

    fn diagnostic(ctx: &RuleContext<Self>, state: &Self::State) -> Option<RuleDiagnostic> {
        let entry = ctx.options().entries.as_ref()?.get(state.entry_index)?;
        let mut diagnostic = match state.kind {
            RestrictedPropertyKind::Exact => RuleDiagnostic::new(
                rule_category!(),
                state.range,
                markup! {
                    "Do not use '"{entry.object.as_deref()?}"."{entry.property.as_deref()?}"'."
                },
            ),
            RestrictedPropertyKind::ObjectOnly => RuleDiagnostic::new(
                rule_category!(),
                state.range,
                markup! {
                    "Do not access properties on '"{entry.object.as_deref()?}"'."
                },
            ),
            RestrictedPropertyKind::PropertyOnly => RuleDiagnostic::new(
                rule_category!(),
                state.range,
                markup! {
                    "Do not use property '"{entry.property.as_deref()?}"'."
                },
            ),
        };

        if let Some(message) = entry.message.as_deref() {
            diagnostic = diagnostic.note(markup! { {message} });
        } else {
            diagnostic = diagnostic
                .note(markup! { "This property is restricted by the lint configuration." });
        }

        if state.kind == RestrictedPropertyKind::ObjectOnly && !entry.allow_properties.is_empty() {
            diagnostic = diagnostic.note(markup! {
                "Only these properties are allowed on '"{entry.object.as_deref()?}"': "{AllowedNames(&entry.allow_properties)}"."
            });
        }

        if state.kind == RestrictedPropertyKind::PropertyOnly && !entry.allow_objects.is_empty() {
            diagnostic = diagnostic.note(markup! {
                "Property '"{entry.property.as_deref()?}"' is only allowed on these objects: "{AllowedNames(&entry.allow_objects)}"."
            });
        }

        diagnostic = diagnostic
            .note(markup! { "Remove this usage to comply with the project's guidelines." });

        Some(diagnostic)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RestrictedPropertyKind {
    Exact,
    ObjectOnly,
    PropertyOnly,
}

#[derive(Debug, Clone)]
struct IdentifierName(TokenText);

impl Display for IdentifierName {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> std::io::Result<()> {
        fmt.write_str(self.0.text())
    }
}

struct AllowedNames<'a>(&'a [Box<str>]);

impl Display for AllowedNames<'_> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> std::io::Result<()> {
        for (index, name) in self.0.iter().enumerate() {
            if index > 0 {
                fmt.write_str(", ")?;
            }
            fmt.write_str(name)?;
        }
        Ok(())
    }
}

/// Checks a property access like `obj.prop` or `obj["prop"]` against the configured entries.
///
/// ## Examples
///
/// ```js
/// require.ensure("./entry")
/// require["ensure"]("./entry")
/// ```
fn inspect_member_expression(
    node: &AnyJsMemberExpression,
    entries: &[RestrictedPropertyEntry],
) -> Option<RestrictedPropertyState> {
    let property_name = node.member_name()?;
    let object = node.object().ok()?.omit_parentheses();
    match_restriction(
        entries,
        identifier_object_name(&object).as_ref(),
        property_name.text(),
        property_name.range(),
    )
}

/// Checks destructuring bindings like `const { prop } = obj` against the configured entries.
///
/// ## Examples
///
/// ```js
/// const { ensure } = require
/// const { callee } = arguments
/// ```
fn inspect_object_binding_pattern(
    node: &JsObjectBindingPattern,
    entries: &[RestrictedPropertyEntry],
) -> Box<[RestrictedPropertyState]> {
    let object_name = object_name_from_binding_pattern(node);
    node.properties()
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|property| match property {
            AnyJsObjectBindingPatternMember::JsObjectBindingPatternProperty(property) => {
                let member = property.member().ok()?;
                match_restriction(
                    entries,
                    object_name.as_ref(),
                    member.name()?.text(),
                    member.range(),
                )
            }
            AnyJsObjectBindingPatternMember::JsObjectBindingPatternShorthandProperty(property) => {
                let token = property
                    .identifier()
                    .ok()?
                    .as_js_identifier_binding()?
                    .name_token()
                    .ok()?;
                match_restriction(
                    entries,
                    object_name.as_ref(),
                    token.text_trimmed(),
                    token.text_trimmed_range(),
                )
            }
            AnyJsObjectBindingPatternMember::JsObjectBindingPatternRest(_)
            | AnyJsObjectBindingPatternMember::JsBogusBinding(_)
            | AnyJsObjectBindingPatternMember::JsMetavariable(_) => None,
        })
        .collect::<Vec<_>>()
        .into_boxed_slice()
}

/// Checks assignment destructuring like `({ prop } = obj)` against the configured entries.
///
/// ## Examples
///
/// ```js
/// ({ callee } = arguments)
/// ({ __defineGetter__ } = foo)
/// ```
fn inspect_object_assignment_pattern(
    node: &JsObjectAssignmentPattern,
    entries: &[RestrictedPropertyEntry],
) -> Box<[RestrictedPropertyState]> {
    let object_name = object_name_from_assignment_pattern(node);
    node.properties()
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|property| match property {
            AnyJsObjectAssignmentPatternMember::JsObjectAssignmentPatternProperty(property) => {
                let member = property.member().ok()?;
                match_restriction(
                    entries,
                    object_name.as_ref(),
                    member.name()?.text(),
                    member.range(),
                )
            }
            AnyJsObjectAssignmentPatternMember::JsObjectAssignmentPatternShorthandProperty(
                property,
            ) => {
                let token = property.identifier().ok()?.name_token().ok()?;
                match_restriction(
                    entries,
                    object_name.as_ref(),
                    token.text_trimmed(),
                    token.text_trimmed_range(),
                )
            }
            AnyJsObjectAssignmentPatternMember::JsObjectAssignmentPatternRest(_)
            | AnyJsObjectAssignmentPatternMember::JsBogusAssignment(_) => None,
        })
        .collect::<Vec<_>>()
        .into_boxed_slice()
}

/// Returns the destructured source object for `const { ... } = expr` when it can be represented as a static name.
///
/// ## Examples
///
/// ```js
/// const { ensure } = require
/// const { __defineGetter__ } = Object
/// ```
fn object_name_from_binding_pattern(node: &JsObjectBindingPattern) -> Option<IdentifierName> {
    let declarator = node.parent::<JsVariableDeclarator>()?;
    let initializer = declarator.initializer()?;
    identifier_object_name(&initializer.expression().ok()?)
}

/// Returns the destructured source object for `({ ... } = expr)` when it can be represented as a static name.
///
/// ## Examples
///
/// ```js
/// ({ callee } = arguments)
/// ({ __defineGetter__ } = Object)
/// ```
fn object_name_from_assignment_pattern(node: &JsObjectAssignmentPattern) -> Option<IdentifierName> {
    let assignment = node.parent::<JsAssignmentExpression>()?;
    match assignment.left().ok()? {
        AnyJsAssignmentPattern::JsObjectAssignmentPattern(pattern) if pattern == *node => {
            identifier_object_name(&assignment.right().ok()?)
        }
        _ => None,
    }
}

/// Returns the object name when the object expression is a plain identifier.
///
/// Configured object names only support valid JavaScript identifiers.
///
/// ## Examples
///
/// ```js
/// require.ensure("./entry")
/// Object.__defineGetter__
/// ```
fn identifier_object_name(expression: &AnyJsExpression) -> Option<IdentifierName> {
    let reference = expression
        .clone()
        .omit_parentheses()
        .as_js_reference_identifier()?;
    Some(IdentifierName(
        reference.value_token().ok()?.token_text_trimmed(),
    ))
}

/// Matches a discovered object/property access against the configured restrictions.
///
/// Exact object/property restrictions take precedence over object-wide and property-wide restrictions.
///
/// ## Examples
///
/// ```js
/// require.ensure("./entry")
/// arguments.callee
/// foo.__defineGetter__
/// ```
fn match_restriction(
    entries: &[RestrictedPropertyEntry],
    object_name: Option<&IdentifierName>,
    property_name: &str,
    range: TextRange,
) -> Option<RestrictedPropertyState> {
    if let Some(object_name) = object_name {
        if let Some((entry_index, _)) = entries.iter().enumerate().find(|(_, entry)| {
            entry
                .object
                .as_deref()
                .is_some_and(|expected| object_name.0.text() == expected)
                && entry.property.as_deref() == Some(property_name)
        }) {
            return Some(RestrictedPropertyState {
                range,
                kind: RestrictedPropertyKind::Exact,
                entry_index,
            });
        }

        if let Some((entry_index, _)) = entries.iter().enumerate().find(|(_, entry)| {
            entry
                .object
                .as_deref()
                .is_some_and(|expected| object_name.0.text() == expected)
                && entry.property.is_none()
                && !entry
                    .allow_properties
                    .iter()
                    .any(|allowed| allowed.as_ref() == property_name)
        }) {
            return Some(RestrictedPropertyState {
                range,
                kind: RestrictedPropertyKind::ObjectOnly,
                entry_index,
            });
        }
    }

    entries
        .iter()
        .enumerate()
        .find(|(_, entry)| {
            entry.object.is_none()
                && entry.property.as_deref() == Some(property_name)
                && !object_name.is_some_and(|object_name| {
                    entry
                        .allow_objects
                        .iter()
                        .any(|allowed| allowed.as_ref() == object_name.0.text())
                })
        })
        .map(|(entry_index, _)| RestrictedPropertyState {
            range,
            kind: RestrictedPropertyKind::PropertyOnly,
            entry_index,
        })
}
