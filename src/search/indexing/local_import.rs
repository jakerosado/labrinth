use futures::TryStreamExt;
use log::info;
use std::collections::HashMap;

use super::IndexingError;
use crate::database::models::{project_item, version_item, ProjectId, VersionId};
use crate::database::redis::RedisPool;
use crate::models;
use crate::models::v2::projects::LegacyProject;
use crate::routes::v2_reroute;
use crate::search::UploadSearchProject;
use sqlx::postgres::PgPool;

pub async fn get_all_ids(
    pool: PgPool,
) -> Result<Vec<(VersionId, ProjectId, String)>, IndexingError> {
    // TODO: Currently org owner is set to be considered owner. It may be worth considering
    // adding a new facetable 'organization' field to the search index, and using that instead,
    // and making owner to be optional.
    let all_visible_ids: Vec<(VersionId, ProjectId, String)> = sqlx::query!(
        "
        SELECT v.id id, m.id mod_id, COALESCE(u.username, ou.username) owner_username
        FROM versions v
        INNER JOIN mods m ON v.mod_id = m.id AND m.status = ANY($2)
        LEFT JOIN team_members tm ON tm.team_id = m.team_id AND tm.is_owner = TRUE AND tm.accepted = TRUE
        LEFT JOIN users u ON tm.user_id = u.id
        LEFT JOIN organizations o ON o.id = m.organization_id
        LEFT JOIN team_members otm ON otm.team_id = o.team_id AND otm.is_owner = TRUE AND otm.accepted = TRUE
        LEFT JOIN users ou ON otm.user_id = ou.id
        WHERE v.status != ANY($1)
        GROUP BY v.id, m.id, u.username, ou.username
        ORDER BY m.id DESC;
        ",
        &*crate::models::projects::VersionStatus::iterator()
            .filter(|x| x.is_hidden())
            .map(|x| x.to_string())
            .collect::<Vec<String>>(),
        &*crate::models::projects::ProjectStatus::iterator()
            .filter(|x| x.is_searchable())
            .map(|x| x.to_string())
            .collect::<Vec<String>>(),
    )
    .fetch_many(&pool)
    .try_filter_map(|e| async move {
        Ok(e.right().map(|m| {
            let project_id: ProjectId = ProjectId(m.mod_id);
            let version_id: VersionId = VersionId(m.id);
            let owner_username = m.owner_username.unwrap_or_default();
            (version_id, project_id, owner_username)
        }))
    })
    .try_collect::<Vec<_>>()
    .await?;

    Ok(all_visible_ids)
}

pub async fn index_local(
    pool: &PgPool,
    redis: &RedisPool,
    visible_ids: HashMap<VersionId, (ProjectId, String)>,
) -> Result<Vec<UploadSearchProject>, IndexingError> {
    info!("Indexing local projects!");
    let project_ids = visible_ids
        .values()
        .map(|(project_id, _)| project_id)
        .cloned()
        .collect::<Vec<_>>();
    let projects: HashMap<_, _> = project_item::Project::get_many_ids(&project_ids, pool, redis)
        .await?
        .into_iter()
        .map(|p| (p.inner.id, p))
        .collect();

    info!("Fetched local projects!");

    let version_ids = visible_ids.keys().cloned().collect::<Vec<_>>();
    let versions: HashMap<_, _> = version_item::Version::get_many(&version_ids, pool, redis)
        .await?
        .into_iter()
        .map(|v| (v.inner.id, v))
        .collect();

    info!("Fetched local versions!");

    let mut uploads = Vec::new();
    // TODO: could possibly clone less here?
    for (version_id, (project_id, owner_username)) in visible_ids {
        let m = projects.get(&project_id);
        let v = versions.get(&version_id);

        let m = match m {
            Some(m) => m,
            None => continue,
        };

        let v = match v {
            Some(v) => v,
            None => continue,
        };

        let version_id: crate::models::projects::VersionId = v.inner.id.into();
        let project_id: crate::models::projects::ProjectId = m.inner.id.into();
        let team_id: crate::models::teams::TeamId = m.inner.team_id.into();
        let organization_id: Option<crate::models::organizations::OrganizationId> =
            m.inner.organization_id.map(|x| x.into());
        let thread_id: crate::models::threads::ThreadId = m.thread_id.into();

        let all_version_ids = m
            .versions
            .iter()
            .map(|v| (*v).into())
            .collect::<Vec<crate::models::projects::VersionId>>();

        let mut additional_categories = m.additional_categories.clone();
        let mut categories = m.categories.clone();

        // Uses version loaders, not project loaders.
        categories.append(&mut v.loaders.clone());

        let display_categories = categories.clone();
        categories.append(&mut additional_categories);

        let version_fields = v.version_fields.clone();
        let unvectorized_loader_fields = v
            .version_fields
            .iter()
            .map(|vf| (vf.field_name.clone(), vf.value.serialize_internal()))
            .collect();
        let mut loader_fields = models::projects::from_duplicate_version_fields(version_fields);
        let license = match m.inner.license.split(' ').next() {
            Some(license) => license.to_string(),
            None => m.inner.license.clone(),
        };

        let open_source = match spdx::license_id(&license) {
            Some(id) => id.is_osi_approved(),
            _ => false,
        };

        // For loaders, get ALL loaders across ALL versions
        let mut loaders = all_version_ids
            .iter()
            .fold(vec![], |mut loaders, version_id| {
                let version = versions.get(&(*version_id).into());
                if let Some(version) = version {
                    loaders.extend(version.loaders.clone());
                }
                loaders
            });
        loaders.sort();
        loaders.dedup();

        // SPECIAL BEHAVIOUR
        // Todo: revisit.
        // For consistency with v2 searching, we consider the loader field 'mrpack_loaders' to be a category.
        // These were previously considered the loader, and in v2, the loader is a category for searching.
        // So to avoid breakage or awkward conversions, we just consider those loader_fields to be categories.
        // The loaders are kept in loader_fields as well, so that no information is lost on retrieval.
        let mrpack_loaders = loader_fields
            .get("mrpack_loaders")
            .cloned()
            .map(|x| {
                x.into_iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        categories.extend(mrpack_loaders);
        if loader_fields.contains_key("mrpack_loaders") {
            categories.retain(|x| *x != "mrpack");
        }

        // SPECIAL BEHAVIOUR:
        // For consitency with v2 searching, we manually input the
        // client_side and server_side fields from the loader fields into
        // separate loader fields.
        // 'client_side' and 'server_side' remain supported by meilisearch even though they are no longer v3 fields.
        let (_, v2_og_project_type) = LegacyProject::get_project_type(&v.project_types);
        let (client_side, server_side) = v2_reroute::convert_side_types_v2(
            &unvectorized_loader_fields,
            Some(&v2_og_project_type),
        );

        if let Ok(client_side) = serde_json::to_value(client_side) {
            loader_fields.insert("client_side".to_string(), vec![client_side]);
        }
        if let Ok(server_side) = serde_json::to_value(server_side) {
            loader_fields.insert("server_side".to_string(), vec![server_side]);
        }

        let gallery = m
            .gallery_items
            .iter()
            .filter(|gi| !gi.featured)
            .map(|gi| gi.image_url.clone())
            .collect::<Vec<_>>();
        let featured_gallery = m
            .gallery_items
            .iter()
            .filter(|gi| gi.featured)
            .map(|gi| gi.image_url.clone())
            .collect::<Vec<_>>();
        let featured_gallery = featured_gallery.first().cloned();

        let usp = UploadSearchProject {
            version_id: version_id.to_string(),
            project_id: project_id.to_string(),
            name: m.inner.name.clone(),
            summary: m.inner.summary.clone(),
            categories,
            follows: m.inner.follows,
            downloads: m.inner.downloads,
            icon_url: m.inner.icon_url.clone(),
            author: owner_username,
            date_created: m.inner.approved.unwrap_or(m.inner.published),
            created_timestamp: m.inner.approved.unwrap_or(m.inner.published).timestamp(),
            date_modified: m.inner.updated,
            modified_timestamp: m.inner.updated.timestamp(),
            license,
            slug: m.inner.slug.clone(),
            project_types: m.project_types.clone(),
            gallery,
            featured_gallery,
            display_categories,
            open_source,
            color: m.inner.color,
            loader_fields,
            license_url: m.inner.license_url.clone(),
            monetization_status: Some(m.inner.monetization_status),
            team_id: team_id.to_string(),
            organization_id: organization_id.map(|x| x.to_string()),
            thread_id: thread_id.to_string(),
            versions: all_version_ids.iter().map(|x| x.to_string()).collect(),
            date_published: m.inner.published,
            date_queued: m.inner.queued,
            status: m.inner.status,
            requested_status: m.inner.requested_status,
            games: m.games.clone(),
            links: m.urls.clone(),
            gallery_items: m.gallery_items.clone(),
            loaders,
        };

        uploads.push(usp);
    }

    Ok(uploads)
}
