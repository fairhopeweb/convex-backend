use std::collections::BTreeMap;

use convex::Value;
use runtime::testing::TestRuntime;
use serde_json::json;

use crate::test_helpers::simulation::SimulationTest;

#[convex_macro::test_runtime]
async fn test_js_thread(rt: TestRuntime) -> anyhow::Result<()> {
    SimulationTest::run(rt.clone(), 2, async |t: SimulationTest| {
        let mut tokens = vec![];
        for js_client in &t.js_clients {
            let token = js_client
                .add_query("basic:count".parse()?, Value::Object(BTreeMap::new()))
                .await?;
            tokens.push(token);
        }

        // Check that the query gets loaded to its initial value.
        let ts = t.server.latest_timestamp().await?;
        for (js_client, token) in t.js_clients.iter().zip(tokens.iter()) {
            js_client.wait_for_server_ts(ts).await?;
            let result = js_client.query_result(token.clone()).await?;
            assert_eq!(result, Some(Value::Float64(0.0)));
        }

        // Issue a server-side mutation and check that the query updates.
        t.server
            .mutation("basic:insertObject".parse()?, vec![json!({})])
            .await??;
        let ts = t.server.latest_timestamp().await?;

        for (js_client, token) in t.js_clients.iter().zip(tokens.iter()) {
            js_client.wait_for_server_ts(ts).await?;
            let result = js_client.query_result(token.clone()).await?;
            assert_eq!(result, Some(Value::Float64(1.0)));
        }

        // Disconnect the network for the first client, issue a server-side mutation,
        // and check that the update only propagates to the second client.
        t.js_clients[0].disconnect_network().await?;
        t.server
            .mutation("basic:insertObject".parse()?, vec![json!({})])
            .await??;
        let ts = t.server.latest_timestamp().await?;

        t.js_clients[1].wait_for_server_ts(ts).await?;
        let result = t.js_clients[0].query_result(tokens[0].clone()).await?;
        assert_eq!(result, Some(Value::Float64(1.0)));
        let result = t.js_clients[1].query_result(tokens[1].clone()).await?;
        assert_eq!(result, Some(Value::Float64(2.0)));

        // Reconnect the network and check that the update propagates to the
        // first client.
        t.js_clients[0].reconnect_network().await?;
        t.js_clients[0].wait_for_server_ts(ts).await?;
        let result = t.js_clients[0].query_result(tokens[0].clone()).await?;
        assert_eq!(result, Some(Value::Float64(2.0)));

        for (js_client, token) in t.js_clients.iter().zip(tokens.iter()) {
            js_client.remove_query(token.clone()).await?;
        }

        Ok(())
    })
    .await
}