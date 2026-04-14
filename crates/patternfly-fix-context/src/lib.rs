//! PatternFly v5 to v6 migration context for the fix engine.
//!
//! Provides framework-specific LLM prompt constraints, priority ordering,
//! and system prompts for the PatternFly component library migration.

use fix_engine::context::FixContext;

/// The ruleset name used by the PatternFly v5→v6 Konveyor rules.
pub const PATTERNFLY_RULESET_NAME: &str = "konveyor-patternfly-v5-to-v6";

/// Fix context for PatternFly v5 to v6 migration.
pub struct PatternFlyV5ToV6Context {
    constraints: Vec<String>,
}

impl PatternFlyV5ToV6Context {
    pub fn new() -> Self {
        Self {
            constraints: vec![
                "NEVER use deep import paths like '@patternfly/react-core/dist/esm/...' or '@patternfly/react-core/next'. Always use the public barrel import '@patternfly/react-core'.".to_string(),
                "NEVER replace PatternFly components (Button, MenuToggle, etc.) with raw HTML elements (<button>, <a>, <div>). If a component still exists in PF6, keep using it.".to_string(),
                "NEVER remove data-ouia-component-id, ouiaId, or other test identifier props unless the migration rule specifically says to.".to_string(),
                "NEVER invent or use component names that are not mentioned in the migration rules or already imported in the file. Only use components explicitly named in the rule message.".to_string(),
                "When adding new components (ModalHeader, ModalBody, ModalFooter, etc.), import them from the same package as the parent component.".to_string(),
                "If the migration rule says a component was \"restructured\" or \"still exists\", keep the component and only restructure its props/children as described. If the rule says a component was \"removed\" and tells you to remove the import, DO remove it and migrate to the replacement described in the rule.".to_string(),
                "When a prop migration says to pass a prop to a child component (e.g., 'actions → pass as children of <ModalFooter>'), you MUST create that child component element, import it, and render the prop value within it.".to_string(),
                "Props can be passed in multiple ways — look for ALL of them when migrating a removed prop:\n  * Direct: `propName={value}`\n  * Conditional spread: `{...(value && { propName: value })}` or `{...(condition && { propName })}` — convert the condition to wrap the new child component, e.g., `{value && <ChildComponent>{value}</ChildComponent>}`\n  * Object spread: `{...props}` — check if the spread object contains the removed prop".to_string(),
            ],
        }
    }
}

impl Default for PatternFlyV5ToV6Context {
    fn default() -> Self {
        Self::new()
    }
}

impl FixContext for PatternFlyV5ToV6Context {
    fn ruleset_name(&self) -> &str {
        PATTERNFLY_RULESET_NAME
    }

    fn migration_description(&self) -> &str {
        "PatternFly v5 to v6 migration"
    }

    fn llm_constraints(&self) -> &[String] {
        &self.constraints
    }

    fn revert_warnings(&self) -> Option<&str> {
        Some(
            "CRITICAL: Do NOT move imports back to '@patternfly/react-core/deprecated' if \
             a previous fix moved them to '@patternfly/react-core'. The migration direction \
             is always FROM deprecated TO the main package, never the reverse.",
        )
    }

    fn fix_priority(&self, rule_id: &str) -> u8 {
        if rule_id.contains("hierarchy-") {
            0 // hierarchy composition: highest priority, restructures component children
        } else if rule_id.contains("component-import-deprecated") {
            1 // structural migration: removed/restructured components
        } else if rule_id.contains("composition") || rule_id.contains("new-sibling") {
            2 // composition: children→prop, new wrapper components
        } else if rule_id.contains("removed")
            || rule_id.contains("renamed")
            || rule_id.contains("type-changed")
            || rule_id.contains("signature-changed")
            || rule_id.contains("prop-value")
        {
            3 // prop-level changes
        } else if rule_id.contains("behavioral")
            || rule_id.contains("dom-structure")
            || rule_id.contains("css-")
            || rule_id.contains("accessibility")
            || rule_id.contains("logic-change")
            || rule_id.contains("render-output")
        {
            4 // informational: DOM/CSS/a11y changes
        } else if rule_id.contains("conformance") {
            5 // review-only: conformance checks
        } else {
            3 // default: treat as prop-level
        }
    }

    fn change_type_examples(&self) -> &str {
        "add/remove/move import, restructure JSX, migrate prop"
    }

    fn verification_prompt(&self) -> Option<&str> {
        Some(
            "VERIFICATION: After making edits, check that EVERY removed prop listed in the migration rules \
             has been migrated to its specified child component. Do NOT declare a migration \"already applied\" \
             unless ALL listed child components are present AND all removed props are accounted for.",
        )
    }

    fn llm_system_prompt(&self) -> String {
        "You are a PatternFly v5 to v6 migration assistant. \
         Given a code snippet and a migration message, output ONLY the corrected \
         code for the affected lines. Output in this exact format:\n\n\
         ```fix\n\
         LINE:<line_number>\n\
         OLD:<exact old text on that line>\n\
         NEW:<replacement text>\n\
         ```\n\n\
         You may output multiple fix blocks. Do not include any explanation outside \
         the fix blocks. Only output fixes for lines that need to change.\n\n\
         IMPORTANT: After applying your fixes, check the import statements at the top \
         of the file. If any imported names are no longer referenced in the file body, \
         remove those specifiers from the import. If removing the last specifier from \
         an import line, remove the entire import line. Output fix blocks for any \
         import cleanup needed."
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ruleset_name() {
        let ctx = PatternFlyV5ToV6Context::new();
        assert_eq!(ctx.ruleset_name(), PATTERNFLY_RULESET_NAME);
    }

    #[test]
    fn test_migration_description() {
        let ctx = PatternFlyV5ToV6Context::new();
        assert_eq!(ctx.migration_description(), "PatternFly v5 to v6 migration");
    }

    #[test]
    fn test_constraints_not_empty() {
        let ctx = PatternFlyV5ToV6Context::new();
        assert!(!ctx.llm_constraints().is_empty());
        // All constraints should mention PatternFly-specific patterns
        assert!(ctx
            .llm_constraints()
            .iter()
            .any(|c| c.contains("@patternfly")));
    }

    #[test]
    fn test_revert_warnings() {
        let ctx = PatternFlyV5ToV6Context::new();
        let warnings = ctx.revert_warnings().unwrap();
        assert!(warnings.contains("deprecated"));
        assert!(warnings.contains("@patternfly/react-core"));
    }

    #[test]
    fn test_priority_hierarchy_first() {
        let ctx = PatternFlyV5ToV6Context::new();
        assert_eq!(ctx.fix_priority("pfv6-hierarchy-modal"), 0);
        assert_eq!(ctx.fix_priority("pfv6-component-import-deprecated"), 1);
        assert_eq!(ctx.fix_priority("pfv6-composition-toolbar"), 2);
        assert_eq!(ctx.fix_priority("pfv6-prop-removed-isActive"), 3);
        assert_eq!(ctx.fix_priority("pfv6-dom-structure-change"), 4);
        assert_eq!(ctx.fix_priority("conformance-check"), 5);
        // sd-conformance-* rules (from structural diff) must also match
        assert_eq!(ctx.fix_priority("sd-conformance-tbody-must-be-in-table"), 5);
        assert_eq!(
            ctx.fix_priority("sd-conformance-toolbaritem-must-be-in-toolbar"),
            5
        );
        assert_eq!(
            ctx.fix_priority("sd-conformance-pagesection-must-be-in-page"),
            5
        );
    }

    #[test]
    fn test_priority_unknown_defaults_to_prop_level() {
        let ctx = PatternFlyV5ToV6Context::new();
        assert_eq!(ctx.fix_priority("some-unknown-rule"), 3);
    }

    #[test]
    fn test_system_prompt() {
        let ctx = PatternFlyV5ToV6Context::new();
        let prompt = ctx.llm_system_prompt();
        assert!(prompt.contains("PatternFly v5 to v6"));
        assert!(prompt.contains("```fix"));
    }
}
