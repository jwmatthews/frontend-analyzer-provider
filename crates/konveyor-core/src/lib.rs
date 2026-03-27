//! Shared Konveyor types for analysis output.
//!
//! This crate defines the common data model used by Konveyor tooling:
//! - `incident`: Incident types representing matched violations in source code
//! - `report`: Konveyor output format types (RuleSet, Violation, etc.)

pub mod incident;
pub mod report;
