//! Capability definitions for the frontend provider.
//!
//! Each capability maps to a distinct analysis domain:
//! - `referenced`: Semantic JS/TS/JSX/TSX symbol search (imports, JSX usage, props, etc.)
//! - `cssclass`: CSS class name search across CSS and JS/TS files
//! - `cssvar`: CSS custom property search
//! - `dependency`: package.json dependency checking

use serde::{Deserialize, Serialize};

/// All capabilities this provider supports.
pub const CAPABILITIES: &[&str] = &["referenced", "cssclass", "cssvar", "dependency"];

/// The location within source code to search for references.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReferenceLocation {
    /// Match import declarations: `import { X } from '...'`
    Import,
    /// Match JSX component usage: `<Button ...>`
    JsxComponent,
    /// Match JSX prop usage: `<Button isActive={...}>`
    JsxProp,
    /// Match function/hook calls: `useButton(...)`
    FunctionCall,
    /// Match type references: `const x: ButtonProps = ...`
    TypeReference,
}

/// Condition for the `referenced` capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferencedCondition {
    /// Pattern to search for (supports regex).
    pub pattern: String,
    /// Optional location filter. If absent, searches all locations.
    pub location: Option<ReferenceLocation>,
    /// Optional component name filter for JSX_PROP location.
    /// When set, only matches props on components whose name matches this pattern.
    pub component: Option<String>,
    /// Optional parent component filter for JSX_COMPONENT location.
    /// When set, only matches components that are direct children of a parent
    /// JSX element whose name matches this pattern.
    pub parent: Option<String>,
    /// Optional parent import source filter for JSX_COMPONENT location.
    /// When set, only matches when the parent JSX component was imported from
    /// a module whose path matches this pattern. Requires `parent` to be set.
    /// Example: `parent_from: "@patternfly/react-core"` ensures the parent
    /// `Button` is from PatternFly, not a custom app component.
    #[serde(rename = "parentFrom", skip_serializing_if = "Option::is_none")]
    pub parent_from: Option<String>,
    /// Optional prop value filter for JSX_PROP location.
    /// When set, only matches props whose value matches this pattern.
    /// Matches against string literal values (e.g., variant="plain") and
    /// JSX expression text (e.g., variant={SelectVariant.checkbox}).
    pub value: Option<String>,
    /// Optional import source path filter for IMPORT location.
    /// When set, only matches imports from modules whose path matches this pattern.
    /// Example: `from: "@patternfly/react-core/deprecated"` matches
    /// `import { Select } from '@patternfly/react-core/deprecated'` but not
    /// `import { Select } from '@patternfly/react-core'`.
    pub from: Option<String>,
    /// Optional file path filter (regex).
    #[serde(rename = "filePattern")]
    pub file_pattern: Option<String>,
}

/// Condition for the `cssclass` capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CssClassCondition {
    /// CSS class name pattern (supports regex).
    pub pattern: String,
    /// Optional file pattern filter (regex).
    #[serde(rename = "filePattern")]
    pub file_pattern: Option<String>,
}

/// Condition for the `cssvar` capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CssVarCondition {
    /// CSS variable pattern (supports regex), e.g. `--pf-v5-.*`
    pub pattern: String,
    /// Optional file pattern filter (regex).
    #[serde(rename = "filePattern")]
    pub file_pattern: Option<String>,
}

/// Condition for the `dependency` capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyCondition {
    /// Dependency name (exact or regex via `nameregex`).
    pub name: Option<String>,
    /// Regex pattern for dependency name.
    pub nameregex: Option<String>,
    /// Match versions <= this.
    pub upperbound: Option<String>,
    /// Match versions >= this.
    pub lowerbound: Option<String>,
}

/// A parsed condition from the Konveyor rule YAML.
#[derive(Debug, Clone)]
pub enum ProviderCondition {
    Referenced(ReferencedCondition),
    CssClass(CssClassCondition),
    CssVar(CssVarCondition),
    Dependency(DependencyCondition),
}

impl ProviderCondition {
    /// Parse a condition from capability name and YAML condition string.
    pub fn parse(capability: &str, condition_yaml: &str) -> anyhow::Result<Self> {
        match capability {
            "referenced" => {
                let cond: ReferencedCondition = serde_yml::from_str(condition_yaml)?;
                Ok(ProviderCondition::Referenced(cond))
            }
            "cssclass" => {
                let cond: CssClassCondition = serde_yml::from_str(condition_yaml)?;
                Ok(ProviderCondition::CssClass(cond))
            }
            "cssvar" => {
                let cond: CssVarCondition = serde_yml::from_str(condition_yaml)?;
                Ok(ProviderCondition::CssVar(cond))
            }
            "dependency" => {
                let cond: DependencyCondition = serde_yml::from_str(condition_yaml)?;
                Ok(ProviderCondition::Dependency(cond))
            }
            _ => anyhow::bail!("Unknown capability: {capability}"),
        }
    }
}
