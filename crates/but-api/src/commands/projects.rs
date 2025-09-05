use crate::error::Error;
use but_api_macros::api_cmd;
use gitbutler_project::{self as projects, ProjectId};
use std::path::PathBuf;

#[api_cmd]
pub fn update_project(project: projects::UpdateRequest) -> Result<projects::Project, Error> {
    Ok(gitbutler_project::update(&project)?)
}

/// Adds an existing git repository as a GitButler project.
/// If the directory is not a git repository, an error is returned.
#[api_cmd]
pub fn add_project(path: PathBuf) -> Result<projects::AddProjectOutcome, Error> {
    Ok(gitbutler_project::add(&path)?)
}

#[api_cmd]
pub fn get_project(
    project_id: ProjectId,
    no_validation: Option<bool>,
) -> Result<projects::Project, Error> {
    if no_validation.unwrap_or(false) {
        Ok(gitbutler_project::get_raw(project_id)?)
    } else {
        Ok(gitbutler_project::get_validated(project_id)?)
    }
}

#[api_cmd]
pub fn delete_project(project_id: ProjectId) -> Result<(), Error> {
    gitbutler_project::delete(project_id).map_err(Into::into)
}
