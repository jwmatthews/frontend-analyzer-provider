pub mod capabilities;

// Re-export fix types from fix-engine-core for backward compatibility.
// This allows existing code that uses `frontend_core::fix::*` to continue working.
pub use fix_engine_core as fix;

// Re-export shared konveyor-core types for convenience
pub use konveyor_core::fix as shared_fix;
pub use konveyor_core::incident;
pub use konveyor_core::report;
pub use konveyor_core::rule;
