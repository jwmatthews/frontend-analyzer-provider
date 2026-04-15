use frontend_core::fix::RemediationPlanReport;
use frontend_core::incident::Incident;
use frontend_core::report::{Category, RuleSet, Violation};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_frontend-analyzer-provider")
}

fn make_incident(
    file_path: &Path,
    line_number: u32,
    message: &str,
    code_snip: &str,
    variables: BTreeMap<String, serde_json::Value>,
) -> Incident {
    Incident {
        file_uri: format!("file://{}", file_path.display()),
        line_number: Some(line_number),
        code_location: None,
        message: message.to_string(),
        code_snip: Some(code_snip.to_string()),
        variables,
        effort: None,
        links: Vec::new(),
        is_dependency_incident: false,
    }
}

fn make_ruleset(violations: BTreeMap<String, Violation>) -> RuleSet {
    RuleSet {
        name: "patternfly-v5-to-v6".to_string(),
        description: "test ruleset".to_string(),
        tags: Vec::new(),
        violations,
        insights: BTreeMap::new(),
        errors: BTreeMap::new(),
        unmatched: Vec::new(),
        skipped: Vec::new(),
    }
}

fn write_analysis(path: &Path, rulesets: &[RuleSet]) {
    fs::write(path, serde_json::to_vec_pretty(rulesets).unwrap()).unwrap();
}

fn read_report(path: &Path) -> RemediationPlanReport {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn run_plan(current_dir: &Path, args: &[&str]) {
    let output = Command::new(bin())
        .current_dir(current_dir)
        .args(args)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "plan command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn plan_writes_report_and_does_not_modify_project() {
    let dir = tempfile::tempdir().unwrap();
    let run_dir = dir.path().join("run");
    let project_root = dir.path().join("project");
    fs::create_dir_all(&run_dir).unwrap();
    fs::create_dir_all(project_root.join("src")).unwrap();

    let file_path = project_root.join("src/App.tsx");
    let original_source =
        "import { Chip } from '@patternfly/react-core';\nconst view = <Chip />;\n";
    fs::write(&file_path, original_source).unwrap();

    let analysis_path = dir.path().join("analysis.json");
    let strategies_path = dir.path().join("fix-strategies.json");

    let incident = make_incident(
        &file_path,
        1,
        "Rename Chip to Label",
        "1  import { Chip } from '@patternfly/react-core';",
        BTreeMap::from([(
            "importedName".to_string(),
            serde_json::Value::String("Chip".to_string()),
        )]),
    );

    let ruleset = make_ruleset(BTreeMap::from([(
        "rename-chip".to_string(),
        Violation {
            description: "Chip renamed".to_string(),
            category: Some(Category::Mandatory),
            labels: vec!["change-type=component-rename".to_string()],
            incidents: vec![incident],
            links: Vec::new(),
            effort: Some(1),
        },
    )]));
    write_analysis(&analysis_path, &[ruleset]);

    fs::write(
        &strategies_path,
        serde_json::to_vec_pretty(&json!({
            "rename-chip": {
                "strategy": "Rename",
                "from": "Chip",
                "to": "Label"
            }
        }))
        .unwrap(),
    )
    .unwrap();

    run_plan(
        &run_dir,
        &[
            "plan",
            project_root.to_str().unwrap(),
            "--input",
            analysis_path.to_str().unwrap(),
            "--strategies",
            strategies_path.to_str().unwrap(),
        ],
    );

    let output_path = run_dir.join("remediation-plan.json");
    assert!(output_path.exists());

    let report = read_report(&output_path);
    assert_eq!(
        report.output_path.canonicalize().unwrap(),
        output_path.canonicalize().unwrap()
    );
    assert_eq!(report.summary.deterministic_fix_count, 1);
    assert_eq!(report.summary.llm_item_count, 0);
    assert_eq!(report.summary.manual_item_count, 0);
    assert_eq!(report.files.len(), 1);
    assert!(report.files[0].deterministic_diff.is_some());
    assert_eq!(report.files[0].items.len(), 1);
    assert_eq!(report.files[0].items[0].rule_id, "rename-chip");

    let post_source = fs::read_to_string(&file_path).unwrap();
    assert_eq!(post_source, original_source);
}

#[test]
fn plan_respects_rules_filter() {
    let dir = tempfile::tempdir().unwrap();
    let run_dir = dir.path().join("run");
    let project_root = dir.path().join("project");
    fs::create_dir_all(&run_dir).unwrap();
    fs::create_dir_all(project_root.join("src")).unwrap();

    let file_path = project_root.join("src/App.tsx");
    fs::write(
        &file_path,
        "import { Chip } from '@patternfly/react-core';\nconst view = <Chip />;\n",
    )
    .unwrap();

    let analysis_path = dir.path().join("analysis.json");
    let strategies_path = dir.path().join("fix-strategies.json");

    let rename_incident = make_incident(
        &file_path,
        1,
        "Rename Chip to Label",
        "1  import { Chip } from '@patternfly/react-core';",
        BTreeMap::from([(
            "importedName".to_string(),
            serde_json::Value::String("Chip".to_string()),
        )]),
    );
    let manual_incident = make_incident(
        &file_path,
        2,
        "Manual structure fix required",
        "2  const view = <Chip />;",
        BTreeMap::new(),
    );

    let ruleset = make_ruleset(BTreeMap::from([
        (
            "rename-chip".to_string(),
            Violation {
                description: "Chip renamed".to_string(),
                category: Some(Category::Mandatory),
                labels: vec!["change-type=component-rename".to_string()],
                incidents: vec![rename_incident],
                links: Vec::new(),
                effort: Some(1),
            },
        ),
        (
            "manual-structure".to_string(),
            Violation {
                description: "Structure changed".to_string(),
                category: Some(Category::Mandatory),
                labels: vec!["change-type=dom-structure".to_string()],
                incidents: vec![manual_incident],
                links: Vec::new(),
                effort: Some(1),
            },
        ),
    ]));
    write_analysis(&analysis_path, &[ruleset]);

    fs::write(
        &strategies_path,
        serde_json::to_vec_pretty(&json!({
            "rename-chip": {
                "strategy": "Rename",
                "from": "Chip",
                "to": "Label"
            }
        }))
        .unwrap(),
    )
    .unwrap();

    run_plan(
        &run_dir,
        &[
            "plan",
            project_root.to_str().unwrap(),
            "--input",
            analysis_path.to_str().unwrap(),
            "--strategies",
            strategies_path.to_str().unwrap(),
            "--rules",
            "rename-chip",
        ],
    );

    let report = read_report(&run_dir.join("remediation-plan.json"));
    assert_eq!(report.summary.violation_count, 1);
    assert_eq!(report.summary.incident_count, 1);
    assert_eq!(report.by_rule.len(), 1);
    assert_eq!(report.by_rule[0].rule_id, "rename-chip");
    assert!(report
        .files
        .iter()
        .flat_map(|file| file.items.iter())
        .all(|item| item.rule_id == "rename-chip"));
}

#[test]
fn plan_writes_valid_empty_report() {
    let dir = tempfile::tempdir().unwrap();
    let run_dir = dir.path().join("run");
    let project_root = dir.path().join("project");
    fs::create_dir_all(&run_dir).unwrap();
    fs::create_dir_all(project_root.join("src")).unwrap();

    fs::write(
        project_root.join("src/App.tsx"),
        "export const App = () => null;\n",
    )
    .unwrap();

    let analysis_path = dir.path().join("analysis.json");
    write_analysis(&analysis_path, &[make_ruleset(BTreeMap::new())]);

    run_plan(
        &run_dir,
        &[
            "plan",
            project_root.to_str().unwrap(),
            "--input",
            analysis_path.to_str().unwrap(),
        ],
    );

    let report = read_report(&run_dir.join("remediation-plan.json"));
    assert_eq!(report.summary.violation_count, 0);
    assert_eq!(report.summary.incident_count, 0);
    assert_eq!(report.summary.deterministic_fix_count, 0);
    assert_eq!(report.summary.llm_item_count, 0);
    assert_eq!(report.summary.manual_item_count, 0);
    assert!(report.files.is_empty());
    assert!(report.by_rule.is_empty());
    assert!(report.provider_errors.is_empty());
    assert!(report.llm_plan.openai_requests.is_empty());
    assert!(report.llm_plan.goose_batches.is_empty());
}
