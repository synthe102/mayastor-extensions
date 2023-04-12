use crate::{
    common::{
        constants::{
            AGENT_CORE_LABEL, CHART_VERSION_LABEL_KEY, DRAIN_FOR_UPGRADE, IO_ENGINE_LABEL, PRODUCT,
        },
        error::{
            DrainStorageNode, EmptyPodNodeName, EmptyPodSpec, EmptyStorageNodeSpec, GetStorageNode,
            ListPodsWithLabel, ListPodsWithLabelAndField, PodDelete, Result, StorageNodeUncordon,
            TooManyIoEnginePods,
        },
        kube_client::KubeClientSet,
        rest_client::RestClientSet,
    },
    upgrade::utils::{all_pods_are_ready, is_rebuilding},
};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    api::{DeleteParams, ListParams, ObjectList},
    ResourceExt,
};
use openapi::models::CordonDrainState;
use snafu::ResultExt;
use std::time::Duration;
use utils::{API_REST_LABEL, ETCD_LABEL};

/// Upgrade data plane by controlled restart of io-engine pods
pub(crate) async fn upgrade_data_plane(
    namespace: String,
    rest_endpoint: String,
    upgrade_from_version: String,
    upgrade_to_version: String,
) -> Result<()> {
    let k8s_client = KubeClientSet::builder()
        .with_namespace(namespace.clone())
        .build()
        .await?;

    let rest_client = RestClientSet::new_with_url(rest_endpoint)?;

    let yet_to_upgrade_io_engine_label_selector =
        format!("{IO_ENGINE_LABEL},{CHART_VERSION_LABEL_KEY}={upgrade_from_version}");
    let io_engine_listparam =
        ListParams::default().labels(yet_to_upgrade_io_engine_label_selector.as_str());
    let namespace = namespace.clone();

    // Validate the control plane pod is up and running before we start.
    verify_control_plane_is_running(namespace.clone(), &k8s_client, &upgrade_to_version).await?;

    let initial_io_engine_pod_list: ObjectList<Pod> = k8s_client
        .pods_api()
        .list(&io_engine_listparam)
        .await
        .context(ListPodsWithLabel {
            label: yet_to_upgrade_io_engine_label_selector,
            namespace: namespace.clone(),
        })?;

    for pod in initial_io_engine_pod_list.iter() {
        // Fetch the node name on which the io-engine pod is running
        let node_name = pod
            .spec
            .as_ref()
            .ok_or(
                EmptyPodSpec {
                    name: pod.name_any(),
                    namespace: namespace.clone(),
                }
                .build(),
            )?
            .node_name
            .as_ref()
            .ok_or(
                EmptyPodNodeName {
                    name: pod.name_any(),
                    namespace: namespace.clone(),
                }
                .build(),
            )?
            .as_str();

        tracing::info!(
            pod.name = %pod.name_any(),
            node.name = %node_name,
            "Upgrade starting for data-plane pod"
        );

        // Issue node drain command
        drain_storage_node(node_name, &rest_client).await?;

        // Wait for any rebuild to complete.
        wait_for_rebuild(node_name, &rest_client).await?;

        // restart the data plane pod
        delete_data_plane_pod(node_name, pod, &k8s_client).await?;

        // Uncordon the drained node
        uncordon_node(node_name, &rest_client).await?;

        // validate the new pod is up and running
        verify_data_plane_pod_is_running(
            node_name,
            namespace.clone(),
            &upgrade_to_version,
            &k8s_client,
        )
        .await?;

        // Validate the control plane pod is up and running
        verify_control_plane_is_running(namespace.clone(), &k8s_client, &upgrade_to_version)
            .await?;
    }
    Ok(())
}

/// Uncordon storage Node.
async fn uncordon_node(node_id: &str, rest_client: &RestClientSet) -> Result<()> {
    let drain_label_for_upgrade: String = DRAIN_FOR_UPGRADE.to_string();
    let sleep_duration = Duration::from_secs(1_u64);
    loop {
        let storage_node =
            rest_client
                .nodes_api()
                .get_node(node_id)
                .await
                .context(GetStorageNode {
                    node_id: node_id.to_string(),
                })?;

        match storage_node
            .into_body()
            .spec
            .ok_or(
                EmptyStorageNodeSpec {
                    node_id: node_id.to_string(),
                }
                .build(),
            )?
            .cordondrainstate
        {
            Some(CordonDrainState::drainedstate(drain_state))
                if drain_state.drainlabels.contains(&drain_label_for_upgrade) =>
            {
                rest_client
                    .nodes_api()
                    .delete_node_cordon(node_id, DRAIN_FOR_UPGRADE)
                    .await
                    .context(StorageNodeUncordon {
                        node_id: node_id.to_string(),
                    })?;

                tracing::info!(node.id = %node_id,
                    label = %DRAIN_FOR_UPGRADE,
                    "Removed drain label from {PRODUCT} Node"
                );
            }
            _ => return Ok(()),
        }
        tokio::time::sleep(sleep_duration).await;
    }
}

/// Issue delete command on dataplane pods.
async fn delete_data_plane_pod(
    node_name: &str,
    pod: &Pod,
    k8s_client: &KubeClientSet,
) -> Result<()> {
    // Deleting the io-engine pod
    let pod_name = pod.name_any();
    tracing::info!(
        pod.name = pod_name.clone(),
        node.name = node_name,
        "Deleting the pod"
    );
    k8s_client
        .pods_api()
        .delete(pod_name.as_str(), &DeleteParams::default())
        .await
        .context(PodDelete {
            name: pod_name,
            node: node_name.to_string(),
        })?;
    tracing::info!(node.name = %node_name, "Pod delete command issued");
    Ok(())
}

/// Wait for all the node drain process to complete.
async fn verify_data_plane_pod_is_running(
    node_name: &str,
    namespace: String,
    upgrade_to_version: &String,
    k8s_client: &KubeClientSet,
) -> Result<()> {
    let duration = Duration::from_secs(5_u64);
    // Validate the new pod is up and running
    tracing::info!(node.name = %node_name, "Waiting for data-plane Pod to come to Ready state...");
    while !data_plane_pod_is_running(node_name, namespace.clone(), upgrade_to_version, k8s_client)
        .await?
    {
        tokio::time::sleep(duration).await;
    }
    Ok(())
}

/// Wait for the rebuild to complete if any.
async fn wait_for_rebuild(node_name: &str, rest_client: &RestClientSet) -> Result<()> {
    // Wait for 60 seconds for any rebuilds to kick in.
    tokio::time::sleep(Duration::from_secs(60_u64)).await;
    while is_rebuilding(rest_client).await? {
        tracing::info!(node.name = %node_name, "Waiting for volume rebuild to complete");
        tokio::time::sleep(Duration::from_secs(10_u64)).await;
    }
    Ok(())
}

/// Issue the node drain command on the node.
async fn drain_storage_node(node_id: &str, rest_client: &RestClientSet) -> Result<()> {
    let drain_label_for_upgrade: String = DRAIN_FOR_UPGRADE.to_string();
    let sleep_duration = Duration::from_secs(5_u64);
    loop {
        let storage_node =
            rest_client
                .nodes_api()
                .get_node(node_id)
                .await
                .context(GetStorageNode {
                    node_id: node_id.to_string(),
                })?;

        match storage_node
            .into_body()
            .spec
            .ok_or(
                EmptyStorageNodeSpec {
                    node_id: node_id.to_string(),
                }
                .build(),
            )?
            .cordondrainstate
        {
            Some(CordonDrainState::drainingstate(drain_state))
                if drain_state.drainlabels.contains(&drain_label_for_upgrade) =>
            {
                tracing::info!(node.id = %node_id, "Waiting for {PRODUCT} Node drain to complete...");
                // Wait for node drain to complete.
                tokio::time::sleep(sleep_duration).await;
            }
            Some(CordonDrainState::drainedstate(drain_state))
                if drain_state.drainlabels.contains(&drain_label_for_upgrade) =>
            {
                tracing::info!(node.id = %node_id, "Drain completed for {PRODUCT} Node");
                return Ok(());
            }
            _ => {
                rest_client
                    .nodes_api()
                    .put_node_drain(node_id, DRAIN_FOR_UPGRADE)
                    .await
                    .context(DrainStorageNode {
                        node_id: node_id.to_string(),
                    })?;

                tracing::info!(node.id = %node_id, "Drain started for {PRODUCT} Node");
            }
        }
    }
}

/// Validate if io-engine DaemonSet Pod is running.
async fn data_plane_pod_is_running(
    node: &str,
    namespace: String,
    upgrade_to_version: &String,
    k8s_client: &KubeClientSet,
) -> Result<bool> {
    let node_name_pod_field = format!("spec.nodeName={node}");
    let pod_label = format!("{IO_ENGINE_LABEL},{CHART_VERSION_LABEL_KEY}={upgrade_to_version}");
    let io_engine_listparam = ListParams::default()
        .labels(pod_label.as_str())
        .fields(node_name_pod_field.as_str());

    let pod_list: ObjectList<Pod> = k8s_client
        .pods_api()
        .list(&io_engine_listparam)
        .await
        .context(ListPodsWithLabelAndField {
            label: pod_label,
            field: node_name_pod_field,
            namespace: namespace.clone(),
        })?;

    if pod_list.items.is_empty() {
        return Ok(false);
    }

    if pod_list.items.len() != 1 {
        TooManyIoEnginePods { node_name: node }.fail()?;
    }

    Ok(all_pods_are_ready(pod_list))
}

async fn verify_control_plane_is_running(
    namespace: String,
    k8s_client: &KubeClientSet,
    upgrade_to_version: &String,
) -> Result<()> {
    let duration = Duration::from_secs(3_u64);
    while !control_plane_is_running(namespace.clone(), k8s_client, upgrade_to_version).await? {
        tokio::time::sleep(duration).await;
    }

    Ok(())
}

/// Validate if control-plane pods are running -- etcd, agent-core, api-rest.
async fn control_plane_is_running(
    namespace: String,
    k8s_client: &KubeClientSet,
    upgrade_to_version: &String,
) -> Result<bool> {
    let agent_core_selector_label =
        format!("{AGENT_CORE_LABEL},{CHART_VERSION_LABEL_KEY}={upgrade_to_version}");
    let pod_list: ObjectList<Pod> = k8s_client
        .pods_api()
        .list(&ListParams::default().labels(agent_core_selector_label.as_str()))
        .await
        .context(ListPodsWithLabel {
            label: AGENT_CORE_LABEL.to_string(),
            namespace: namespace.clone(),
        })?;
    let core_is_ready = all_pods_are_ready(pod_list);

    let api_rest_selector_label =
        format!("{API_REST_LABEL},{CHART_VERSION_LABEL_KEY}={upgrade_to_version}");
    let pod_list: ObjectList<Pod> = k8s_client
        .pods_api()
        .list(&ListParams::default().labels(api_rest_selector_label.as_str()))
        .await
        .context(ListPodsWithLabel {
            label: API_REST_LABEL.to_string(),
            namespace: namespace.clone(),
        })?;
    let rest_is_ready = all_pods_are_ready(pod_list);

    let pod_list: ObjectList<Pod> = k8s_client
        .pods_api()
        .list(&ListParams::default().labels(ETCD_LABEL))
        .await
        .context(ListPodsWithLabel {
            label: ETCD_LABEL.to_string(),
            namespace: namespace.clone(),
        })?;
    let etcd_is_ready = all_pods_are_ready(pod_list);

    Ok(core_is_ready && rest_is_ready && etcd_is_ready)
}