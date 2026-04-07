//! sea-query Iden enums for tc_tasks, projects, settings.
//!
//! IdenStatic gives compile-time string constants for column/table names,
//! which sea-query uses to build parameterized SQL with correct PG type OIDs.

use sea_query::IdenStatic;

#[derive(Debug, Clone, Copy, PartialEq, Eq, IdenStatic)]
#[iden(rename = "tc_tasks")]
pub(super) enum TcTasks {
    Table,
    Id,
    Data,
    Status,
    Description,
    Priority,
    EntryAt,
    ModifiedAt,
    DueAt,
    ScheduledAt,
    StartAt,
    EndAt,
    WaitAt,
    ParentId,
    ProjectId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, IdenStatic)]
#[iden(rename = "projects")]
pub(super) enum Projects {
    Table,
    Id,
    Name,
    CreatedAt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, IdenStatic)]
#[iden(rename = "settings")]
pub(super) enum Settings {
    Table,
    TcConfig,
}
