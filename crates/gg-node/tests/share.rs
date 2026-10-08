//! A laboratory's lab node shared between researchers (ADR-0041, A7): its owner grants the use of
//! it; a researcher allowed to use it attaches it to projects they belong to; the node works only
//! for attached projects; it cannot be detached while it still works for a project.

mod common;

use common::*;
use gg_core::*;
use serde_json::json;

#[tokio::test]
async fn a_node_works_only_for_the_projects_its_users_attached() {
    let env = start().await;
    let (ada, ada_project) = researcher(&env, "Ada").await;
    let (bob, bob_project) = researcher(&env, "Bob").await;
    let dir = tempfile::tempdir().unwrap();
    // Ada's node, attached to her project.
    let node = start_node(&env, &ada, ada_project, &dir.path().join("node")).await;
    let id = node.id();
    let (s, me) = env.get(&ada, "/researcher/nodes").await;
    assert_eq!(s, 200, "{me}");
    let bob_id = {
        let (s, b) = env.get(&bob, "/researcher/me").await;
        assert_eq!(s, 200, "{b}");
        b["researcher_id"].as_str().unwrap().to_owned()
    };

    // Bob may not use it until Ada allows him: the node looks unknown to him.
    let attach = |token: &str, project: ProjectId| {
        let token = token.to_owned();
        let env = &env;
        async move {
            env.post(
                &token,
                &format!("/researcher/projects/{project}/nodes"),
                json!({ "node_id": id }),
            )
            .await
        }
    };
    let (s, body) = attach(&bob, bob_project).await;
    assert_eq!(s, 404, "{body}");
    // Only the owner grants.
    let (s, _) = env
        .post(
            &bob,
            &format!("/researcher/nodes/{id}/users"),
            json!({ "researcher_id": bob_id }),
        )
        .await;
    assert_eq!(s, 404);
    let (s, body) = env
        .post(
            &ada,
            &format!("/researcher/nodes/{id}/users"),
            json!({ "researcher_id": bob_id }),
        )
        .await;
    assert_eq!(s, 200, "{body}");
    let (s, users) = env
        .get(&ada, &format!("/researcher/nodes/{id}/users"))
        .await;
    assert_eq!(s, 200);
    assert_eq!(users, json!([bob_id]));
    // Bob sees it among his usable nodes, and attaches it to his own project only.
    let (s, list) = env.get(&bob, "/researcher/nodes").await;
    assert_eq!(s, 200);
    assert_eq!(list[0]["node_id"], json!(id));
    let (s, body) = attach(&bob, ada_project).await;
    assert_eq!(s, 404, "{body}");
    let (s, body) = attach(&bob, bob_project).await;
    assert_eq!(s, 200, "{body}");
    let (s, nodes) = env
        .get(&bob, &format!("/researcher/projects/{bob_project}/nodes"))
        .await;
    assert_eq!(s, 200);
    assert_eq!(nodes[0]["node_id"], json!(id));

    // The node now holds for both projects, and for no other.
    let (_, carol_project) = researcher(&env, "Carol").await;
    let path = dir.path().join("data");
    std::fs::write(&path, vec![3u8; 70_000]).unwrap();
    node.hold(ada_project, &path, "").await.unwrap();
    // A 1×1 matrix, the input of Bob's job below.
    let one = gg_protocol::tensor_file::TensorF32::new(vec![1, 1], vec![2.0]).unwrap();
    std::fs::write(&path, one.encode()).unwrap();
    let bob_data = node.hold(bob_project, &path, "ggt1-f32").await.unwrap();
    std::fs::write(&path, vec![5u8; 70_000]).unwrap();
    let e = node.hold(carol_project, &path, "").await.unwrap_err();
    assert!(e.to_string().contains("404"), "{e}");

    // Withdrawing Bob's use keeps what he attached; he cannot attach anew.
    let (s, body) = env
        .post(
            &ada,
            &format!("/researcher/nodes/{id}/users/{bob_id}/remove"),
            json!({}),
        )
        .await;
    assert_eq!(s, 200, "{body}");
    let (s, nodes) = env
        .get(&bob, &format!("/researcher/projects/{bob_project}/nodes"))
        .await;
    assert_eq!(s, 200);
    assert_eq!(nodes.as_array().unwrap().len(), 1);

    // A job whose outputs the node pulls keeps it attached until the job is over.
    let def = gg_compute::catalog::matmul_definition(
        1,
        1,
        1,
        gg_protocol::messages::ArtifactRef {
            hash: bob_data.hash,
            size: bob_data.size,
        },
        gg_protocol::messages::ArtifactRef {
            hash: bob_data.hash,
            size: bob_data.size,
        },
    )
    .unwrap();
    let (s, job) = env
        .post(
            &bob,
            &format!("/researcher/projects/{bob_project}/jobs"),
            json!({ "name": "mm", "tasks": [def], "output_holder": id }),
        )
        .await;
    assert_eq!(s, 200, "{job}");
    let detach = format!("/researcher/projects/{bob_project}/nodes/{id}/detach");
    let (s, body) = env.post(&bob, &detach, json!({})).await;
    assert_eq!(s, 409, "{body}");
    let (s, body) = env
        .post(
            &bob,
            &format!(
                "/researcher/jobs/{}/cancel",
                job["job_id"].as_str().unwrap()
            ),
            json!({}),
        )
        .await;
    assert_eq!(s, 200, "{body}");
    let (s, body) = env.post(&bob, &detach, json!({})).await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["detached"], json!(true));
    // Detached: the node no longer holds for Bob's project, and a job may not name it.
    std::fs::write(&path, vec![6u8; 70_000]).unwrap();
    let e = node.hold(bob_project, &path, "").await.unwrap_err();
    assert!(e.to_string().contains("404"), "{e}");
    let (s, body) = env
        .post(
            &bob,
            &format!("/researcher/projects/{bob_project}/jobs"),
            json!({ "name": "mm", "tasks": [def], "output_holder": id }),
        )
        .await;
    assert_eq!(s, 422, "{body}");
    let (s, body) = attach(&bob, bob_project).await;
    assert_eq!(s, 404, "{body}");

    node.stop().await;
    env.stop().await;
}
