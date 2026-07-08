//! Typed Linear GraphQL operations over the [`LinearTransport`] seam.
//!
//! Documents use GraphQL VARIABLES exclusively — caller data (issue keys, comment
//! bodies) is never string-interpolated into a document. GraphQL-level errors
//! (HTTP 200 + `errors[]`) map to `LaneError::Network` carrying the first error's
//! message only (truncated) — never a response dump.

use serde_json::{json, Value};

use super::transport::{linear_to_lane, LinearTransport};
use crate::error::LaneError;
use crate::model::PullIssue;
use crate::secrets::SecretValue;

const VIEWER_ISSUES_QUERY: &str = "query($first: Int!) { viewer { assignedIssues(first: $first, orderBy: updatedAt, filter: { state: { type: { nin: [\"completed\", \"canceled\"] } } }) { nodes { identifier title url updatedAt state { name type } } } } }";

/// `issue(id:)` accepts the human identifier (e.g. `ZER-85`) as well as the UUID —
/// documented Linear behavior. If that ever regresses, swap the lookup for
/// `issues(filter: { team: { key: { eq } }, number: { eq } })` inside this module.
const ISSUE_BY_KEY_QUERY: &str = "query($key: String!) { issue(id: $key) { identifier title url updatedAt state { name type } assignee { displayName } } }";

const PREFLIGHT_ISSUE_QUERY: &str =
    "query($key: String!) { issue(id: $key) { id comments(first: 100) { nodes { body } } } }";

const COMMENT_CREATE_MUTATION: &str = "mutation($issueId: String!, $body: String!) { commentCreate(input: { issueId: $issueId, body: $body }) { success comment { url } } }";

/// One issue joined with its assignee (board enrichment shape).
#[derive(Debug, Clone)]
pub struct IssueWithAssignee {
    pub issue: PullIssue,
    pub assignee: Option<String>,
}

/// Preflight facts for the gated closeout write.
#[derive(Debug, Clone)]
pub struct PreflightIssue {
    /// The issue UUID `commentCreate` requires.
    pub uuid: String,
    /// True iff a recent comment already carries the closeout marker (the dedupe
    /// authority when checked inside the publish lock). Scans the first comment
    /// page (100) — a marker buried deeper on a pathologically chatty issue would
    /// re-post, and the marker makes such a duplicate self-identifying.
    pub already_posted: bool,
}

/// A created comment.
#[derive(Debug, Clone)]
pub struct CommentRef {
    pub url: Option<String>,
}

/// POST one GraphQL document; surface transport and GraphQL-level errors uniformly.
fn post(
    transport: &dyn LinearTransport,
    url: &str,
    auth: &SecretValue,
    query: &str,
    variables: Value,
) -> Result<Value, LaneError> {
    let body = json!({ "query": query, "variables": variables });
    let resp = transport
        .post_json(url, auth, &body)
        .map_err(linear_to_lane)?;
    if let Some(errors) = resp.get("errors").and_then(Value::as_array) {
        if !errors.is_empty() {
            let msg: String = errors[0]
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown GraphQL error")
                .chars()
                .take(200)
                .collect();
            return Err(LaneError::Network(format!("linear graphql: {msg}")));
        }
    }
    Ok(resp)
}

fn issue_field(node: &Value, pointer: &str) -> Result<String, LaneError> {
    node.pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| LaneError::Network(format!("linear graphql: issue node missing {pointer}")))
}

fn parse_issue(node: &Value) -> Result<PullIssue, LaneError> {
    Ok(PullIssue {
        identifier: issue_field(node, "/identifier")?,
        title: issue_field(node, "/title")?,
        state: issue_field(node, "/state/name")?,
        state_type: issue_field(node, "/state/type")?,
        url: issue_field(node, "/url")?,
        updated_at: issue_field(node, "/updatedAt")?,
    })
}

/// The viewer's assigned, non-completed/canceled issues, most recently updated first
/// (the `lane pull` read).
pub fn fetch_viewer_issues(
    transport: &dyn LinearTransport,
    url: &str,
    auth: &SecretValue,
    limit: u32,
) -> Result<Vec<PullIssue>, LaneError> {
    let resp = post(
        transport,
        url,
        auth,
        VIEWER_ISSUES_QUERY,
        json!({ "first": limit }),
    )?;
    let nodes = resp
        .pointer("/data/viewer/assignedIssues/nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            LaneError::Network("linear graphql: response missing viewer.assignedIssues".to_string())
        })?;
    nodes.iter().map(parse_issue).collect()
}

/// One issue by human identifier (board enrichment). `Ok(None)` = no such issue
/// (a miss, not an error).
pub fn fetch_issue_by_key(
    transport: &dyn LinearTransport,
    url: &str,
    auth: &SecretValue,
    key: &str,
) -> Result<Option<IssueWithAssignee>, LaneError> {
    let resp = post(
        transport,
        url,
        auth,
        ISSUE_BY_KEY_QUERY,
        json!({ "key": key }),
    )?;
    let node = match resp.pointer("/data/issue") {
        None | Some(Value::Null) => return Ok(None),
        Some(v) => v,
    };
    Ok(Some(IssueWithAssignee {
        issue: parse_issue(node)?,
        assignee: node
            .pointer("/assignee/displayName")
            .and_then(Value::as_str)
            .map(str::to_string),
    }))
}

/// The gated-write preflight: resolve the issue UUID and scan recent comments for
/// the closeout `marker` in ONE query. `Ok(None)` = no such issue.
pub fn preflight_issue(
    transport: &dyn LinearTransport,
    url: &str,
    auth: &SecretValue,
    key: &str,
    marker: &str,
) -> Result<Option<PreflightIssue>, LaneError> {
    let resp = post(
        transport,
        url,
        auth,
        PREFLIGHT_ISSUE_QUERY,
        json!({ "key": key }),
    )?;
    let node = match resp.pointer("/data/issue") {
        None | Some(Value::Null) => return Ok(None),
        Some(v) => v,
    };
    let uuid = issue_field(node, "/id")?;
    let already_posted = node
        .pointer("/comments/nodes")
        .and_then(Value::as_array)
        .map(|nodes| {
            nodes.iter().any(|c| {
                c.get("body")
                    .and_then(Value::as_str)
                    .is_some_and(|b| b.contains(marker))
            })
        })
        .unwrap_or(false);
    Ok(Some(PreflightIssue {
        uuid,
        already_posted,
    }))
}

/// The one wired Linear mutation: create the closeout comment (label/field writes
/// ride this same seam in a later slice).
pub fn post_comment(
    transport: &dyn LinearTransport,
    url: &str,
    auth: &SecretValue,
    issue_uuid: &str,
    body_markdown: &str,
) -> Result<CommentRef, LaneError> {
    let resp = post(
        transport,
        url,
        auth,
        COMMENT_CREATE_MUTATION,
        json!({ "issueId": issue_uuid, "body": body_markdown }),
    )?;
    let success = resp
        .pointer("/data/commentCreate/success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !success {
        return Err(LaneError::Network(
            "linear graphql: commentCreate did not report success".to_string(),
        ));
    }
    Ok(CommentRef {
        url: resp
            .pointer("/data/commentCreate/comment/url")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct FakeTransport {
        responses: RefCell<Vec<Value>>,
        bodies: RefCell<Vec<Value>>,
    }
    impl FakeTransport {
        fn returning(responses: Vec<Value>) -> Self {
            Self {
                responses: RefCell::new(responses),
                bodies: RefCell::new(Vec::new()),
            }
        }
    }
    impl LinearTransport for FakeTransport {
        fn post_json(
            &self,
            _url: &str,
            _auth: &SecretValue,
            body: &Value,
        ) -> Result<Value, super::super::transport::TransportError> {
            self.bodies.borrow_mut().push(body.clone());
            Ok(self.responses.borrow_mut().remove(0))
        }
    }

    fn auth() -> SecretValue {
        SecretValue::new("test-key")
    }

    #[test]
    fn viewer_issues_parse_and_use_variables() {
        let t = FakeTransport::returning(vec![json!({
            "data": { "viewer": { "assignedIssues": { "nodes": [
                { "identifier": "ZER-85", "title": "t", "url": "u",
                  "updatedAt": "2026-07-08T00:00:00Z",
                  "state": { "name": "In Progress", "type": "started" } }
            ] } } }
        })]);
        let issues = fetch_viewer_issues(&t, "https://x/graphql", &auth(), 7).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].identifier, "ZER-85");
        assert_eq!(issues[0].state_type, "started");
        let body = &t.bodies.borrow()[0];
        assert_eq!(body.pointer("/variables/first"), Some(&json!(7)));
        // Caller data rides variables; the document is a fixed string.
        assert!(body
            .pointer("/query")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("$first"));
    }

    #[test]
    fn graphql_errors_map_concisely() {
        let t = FakeTransport::returning(vec![json!({
            "errors": [ { "message": "Entity not found: Issue" } ],
            "data": null
        })]);
        let err = fetch_viewer_issues(&t, "https://x/graphql", &auth(), 1).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Entity not found"));
        assert!(matches!(err, LaneError::Network(_)));
    }

    #[test]
    fn issue_by_key_null_is_a_miss() {
        let t = FakeTransport::returning(vec![json!({ "data": { "issue": null } })]);
        let got = fetch_issue_by_key(&t, "https://x/graphql", &auth(), "ZER-404").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn preflight_finds_marker_in_comments() {
        let resp = json!({ "data": { "issue": {
            "id": "uuid-1",
            "comments": { "nodes": [
                { "body": "unrelated" },
                { "body": "closeout…\n\nlane-closeout: zer-85@2026-07-08T10:52:12Z" }
            ] } } } });
        let t = FakeTransport::returning(vec![resp.clone(), resp]);
        let hit = preflight_issue(
            &t,
            "https://x/graphql",
            &auth(),
            "ZER-85",
            "lane-closeout: zer-85@2026-07-08T10:52:12Z",
        )
        .unwrap()
        .unwrap();
        assert_eq!(hit.uuid, "uuid-1");
        assert!(hit.already_posted);
        let miss = preflight_issue(
            &t,
            "https://x/graphql",
            &auth(),
            "ZER-85",
            "lane-closeout: zer-85@2099-01-01T00:00:00Z",
        )
        .unwrap()
        .unwrap();
        assert!(!miss.already_posted, "different generation must not dedupe");
    }

    #[test]
    fn comment_create_success_and_failure() {
        let t = FakeTransport::returning(vec![
            json!({ "data": { "commentCreate": { "success": true, "comment": { "url": "https://linear.app/c/1" } } } }),
            json!({ "data": { "commentCreate": { "success": false } } }),
        ]);
        let ok = post_comment(&t, "https://x/graphql", &auth(), "uuid-1", "body").unwrap();
        assert_eq!(ok.url.as_deref(), Some("https://linear.app/c/1"));
        let err = post_comment(&t, "https://x/graphql", &auth(), "uuid-1", "body").unwrap_err();
        assert!(err.to_string().contains("did not report success"));
    }
}
