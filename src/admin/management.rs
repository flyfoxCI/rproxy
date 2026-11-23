use super::*;
use crate::managed_types::{AdminRelease, ProxyAction, SourceChange, SourceInput, UserChange};
use serde::de::DeserializeOwned;
use std::collections::HashMap;

fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    serde_json::from_slice(body)
        .map_err(|_| anyhow::anyhow!("invalid management request fields or types"))
}
fn page(mut items: Vec<Value>, query: &HashMap<String, String>) -> Result<Value> {
    let limit = query
        .get("limit")
        .map(|s| s.parse::<usize>())
        .transpose()
        .map_err(|_| anyhow::anyhow!("invalid list limit"))?
        .unwrap_or(100);
    let offset = query
        .get("offset")
        .map(|s| s.parse::<usize>())
        .transpose()
        .map_err(|_| anyhow::anyhow!("invalid list offset"))?
        .unwrap_or(0);
    if limit == 0 || limit > 1000 || offset > 1_000_000 {
        bail!("invalid pagination");
    }
    let total = items.len();
    let items: Vec<_> = items.drain(..).skip(offset).take(limit).collect();
    Ok(json!({"items":items,"total":total,"offset":offset,"limit":limit}))
}
fn flag(query: &HashMap<String, String>, name: &str) -> Result<bool> {
    match query.get(name).map(String::as_str) {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        _ => bail!("invalid boolean query"),
    }
}
fn filtered(items: &mut Vec<Value>, query: &HashMap<String, String>, fields: &[&str]) {
    items.retain(|item| {
        fields.iter().all(|field| {
            query.get(*field).is_none_or(|wanted| {
                item.get(*field)
                    .is_some_and(|value| value.as_str() == Some(wanted))
            })
        })
    });
}
async fn route(
    method: &Method,
    path: &str,
    query: &HashMap<String, String>,
    body: &[u8],
    state: &Arc<State>,
) -> Result<Option<Response<Full<Bytes>>>> {
    let store = &state.router.store;
    if method == Method::GET && path == "/v1/info" {
        return Ok(Some(response(
            StatusCode::OK,
            json!({"version":env!("CARGO_PKG_VERSION"),"management_version":1,"capabilities":["users","sources","proxies","sessions"],"auth_required":state.auth.required()}),
        )));
    }
    if path == "/v1/users" {
        if method == Method::GET {
            let items = store
                .users()
                .await?
                .into_iter()
                .map(serde_json::to_value)
                .collect::<serde_json::Result<Vec<_>>>()?;
            return Ok(Some(response(StatusCode::OK, page(items, query)?)));
        }
        if method == Method::POST {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct NewUser {
                name: String,
                password: String,
                businesses: Vec<String>,
            }
            let request: NewUser = decode(body)?;
            let view = store
                .create_user(request.name, request.password, request.businesses)
                .await?;
            return Ok(Some(response(
                StatusCode::CREATED,
                serde_json::to_value(view)?,
            )));
        }
    }
    if let Some(name) = path.strip_prefix("/v1/users/") {
        if !crate::config::valid_identifier(name) {
            bail!("invalid user identifier");
        }
        let view = if method == Method::GET {
            store.user(name.into()).await?
        } else if method == Method::PATCH {
            Some(
                store
                    .update_user(name.into(), decode::<UserChange>(body)?)
                    .await?,
            )
        } else if method == Method::DELETE {
            Some(
                store
                    .remove_user(name.into(), flag(query, "disconnect")?)
                    .await?,
            )
        } else {
            return Ok(None);
        };
        return Ok(Some(match view {
            Some(view) => response(StatusCode::OK, serde_json::to_value(view)?),
            None => error(StatusCode::NOT_FOUND, "user not found"),
        }));
    }
    if method == Method::GET && path == "/v1/sources/export" {
        return Ok(Some(response(
            StatusCode::OK,
            store
                .export_sources(flag(query, "include_secrets")?)
                .await?,
        )));
    }
    if path == "/v1/sources" {
        if method == Method::GET {
            let items = store
                .sources()
                .await?
                .into_iter()
                .map(serde_json::to_value)
                .collect::<serde_json::Result<Vec<_>>>()?;
            return Ok(Some(response(StatusCode::OK, page(items, query)?)));
        }
        if method == Method::POST {
            return Ok(Some(response(
                StatusCode::CREATED,
                serde_json::to_value(store.create_source(decode::<SourceInput>(body)?).await?)?,
            )));
        }
    }
    if let Some(name) = path.strip_prefix("/v1/sources/") {
        if let Some(name) = name.strip_suffix("/run") {
            if method != Method::POST {
                return Ok(None);
            }
            if !crate::config::valid_identifier(name) {
                bail!("invalid source identifier");
            }
            return Ok(Some(response(
                StatusCode::ACCEPTED,
                store.run_source(name.into()).await?,
            )));
        }
        if !crate::config::valid_identifier(name) {
            bail!("invalid source identifier");
        }
        let view = if method == Method::GET {
            store.source(name.into()).await?
        } else if method == Method::PATCH {
            Some(
                store
                    .update_source(name.into(), decode::<SourceChange>(body)?)
                    .await?,
            )
        } else if method == Method::DELETE {
            Some(store.remove_source(name.into()).await?)
        } else {
            return Ok(None);
        };
        return Ok(Some(match view {
            Some(view) => response(StatusCode::OK, serde_json::to_value(view)?),
            None => error(StatusCode::NOT_FOUND, "source not found"),
        }));
    }
    if method == Method::GET && path == "/v1/proxies" {
        let mut items = store.proxy_views().await?;
        filtered(&mut items, query, &["protocol", "state"]);
        if let Some(source) = query.get("source") {
            items.retain(|item| {
                item["sources"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|name| name.as_str() == Some(source)))
            });
        }
        return Ok(Some(response(StatusCode::OK, page(items, query)?)));
    }
    if method == Method::GET && path == "/v1/proxies/show" {
        let id = query.get("id").context("proxy id is required")?;
        return Ok(Some(
            match store
                .proxy_views()
                .await?
                .into_iter()
                .find(|item| item["id"].as_str() == Some(id))
            {
                Some(item) => response(StatusCode::OK, item),
                None => error(StatusCode::NOT_FOUND, "proxy not found"),
            },
        ));
    }
    if method == Method::POST && path == "/v1/proxies/action" {
        return Ok(Some(response(
            StatusCode::OK,
            serde_json::to_value(store.proxy_action(decode::<ProxyAction>(body)?).await?)?,
        )));
    }
    if method == Method::GET && path == "/v1/sessions" {
        let mut items = store.managed_sessions().await?;
        filtered(&mut items, query, &["business"]);
        if let Some(owner) = query.get("owner") {
            items.retain(|item| {
                item["owner"].as_str() == Some(owner) || item["owner_name"].as_str() == Some(owner)
            });
        }
        return Ok(Some(response(StatusCode::OK, page(items, query)?)));
    }
    if method == Method::POST && path == "/v1/sessions/release" {
        let value: Value = decode(body)?;
        if value.get("business").is_some() {
            let impact = store.admin_release(decode::<AdminRelease>(body)?).await?;
            let mut value = serde_json::to_value(&impact)?;
            value["released_or_draining"] = impact.applied.into();
            return Ok(Some(response(StatusCode::OK, value)));
        }
    }
    Ok(None)
}
pub(super) async fn handle(
    method: &Method,
    path: &str,
    query: &str,
    body: &[u8],
    state: &Arc<State>,
) -> Option<Response<Full<Bytes>>> {
    let query: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    match route(method, path, &query, body, state).await {
        Ok(response) => response,
        Err(error) => Some(super::error(StatusCode::CONFLICT, &error.to_string())),
    }
}
