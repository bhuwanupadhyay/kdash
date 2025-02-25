use std::fmt;

use anyhow::anyhow;
use k8s_openapi::{
  api::{
    apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet},
    batch::v1::{CronJob, Job},
    core::v1::{
      ConfigMap, Namespace, Node, PersistentVolume, PersistentVolumeClaim, Pod,
      ReplicationController, Secret, Service, ServiceAccount,
    },
    networking::v1::Ingress,
    rbac::v1::{ClusterRole, ClusterRoleBinding, Role, RoleBinding},
    storage::v1::StorageClass,
  },
  NamespaceResourceScope,
};
use kube::{
  api::{ListMeta, ListParams, ObjectList},
  config::Kubeconfig,
  Api, Resource as ApiResource,
};
use kubectl_view_allocations::{
  extract_allocatable_from_nodes, extract_allocatable_from_pods,
  extract_utilizations_from_pod_metrics, make_qualifiers, metrics::PodMetrics, Resource,
};
use serde::de::DeserializeOwned;

use super::Network;
use crate::app::{
  configmaps::KubeConfigMap,
  contexts,
  cronjobs::KubeCronJob,
  daemonsets::KubeDaemonSet,
  deployments::KubeDeployment,
  ingress::KubeIngress,
  jobs::KubeJob,
  metrics::{self, KubeNodeMetrics},
  nodes::KubeNode,
  ns::KubeNs,
  pods::KubePod,
  pvcs::KubePVC,
  pvs::KubePV,
  replicasets::KubeReplicaSet,
  replication_controllers::KubeReplicationController,
  roles::{KubeClusterRole, KubeClusterRoleBinding, KubeRole, KubeRoleBinding},
  secrets::KubeSecret,
  serviceaccounts::KubeSvcAcct,
  statefulsets::KubeStatefulSet,
  storageclass::KubeStorageClass,
  svcs::KubeSvc,
};

impl<'a> Network<'a> {
  pub async fn get_kube_config(&self) {
    match Kubeconfig::read() {
      Ok(config) => {
        let mut app = self.app.lock().await;
        let selected_ctx = app.data.selected.context.to_owned();
        app.set_contexts(contexts::get_contexts(&config, selected_ctx));
        app.data.kubeconfig = Some(config);
      }
      Err(e) => {
        self
          .handle_error(anyhow!("Failed to load Kubernetes config. {:?}", e))
          .await;
      }
    }
  }

  pub async fn get_node_metrics(&self) {
    let api_node_metrics: Api<metrics::NodeMetrics> = Api::all(self.client.clone());

    match api_node_metrics.list(&ListParams::default()).await {
      Ok(node_metrics) => {
        let mut app = self.app.lock().await;

        let items = node_metrics
          .iter()
          .map(|metric| KubeNodeMetrics::from_api(metric, &app))
          .collect();

        app.data.node_metrics = items;
      }
      Err(_) => {
        let mut app = self.app.lock().await;
        app.data.node_metrics = vec![];
        // lets not show error as it will always be showing up and be annoying
        // TODO may be show once and then disable polling
      }
    };
  }

  pub async fn get_utilizations(&self) {
    let mut resources: Vec<Resource> = vec![];

    let api: Api<Node> = Api::all(self.client.clone());
    match api.list(&ListParams::default()).await {
      Ok(node_list) => {
        if let Err(e) = extract_allocatable_from_nodes(node_list, &mut resources).await {
          self
            .handle_error(anyhow!(
              "Failed to extract node allocation metrics. {:?}",
              e
            ))
            .await;
        }
      }
      Err(e) => {
        self
          .handle_error(anyhow!(
            "Failed to extract node allocation metrics. {:?}",
            e
          ))
          .await
      }
    }

    let api: Api<Pod> = self.get_namespaced_api().await;
    match api.list(&ListParams::default()).await {
      Ok(pod_list) => {
        if let Err(e) = extract_allocatable_from_pods(pod_list, &mut resources).await {
          self
            .handle_error(anyhow!("Failed to extract pod allocation metrics. {:?}", e))
            .await;
        }
      }
      Err(e) => {
        self
          .handle_error(anyhow!("Failed to extract pod allocation metrics. {:?}", e))
          .await
      }
    }

    let api_pod_metrics: Api<PodMetrics> = Api::all(self.client.clone());

    match api_pod_metrics
    .list(&ListParams::default())
    .await
    {
      Ok(pod_metrics) => {
        if let Err(e) = extract_utilizations_from_pod_metrics(pod_metrics, &mut resources).await {
          self.handle_error(anyhow!("Failed to extract pod utilization metrics. {:?}", e)).await;
        }
      }
      Err(_e) => self.handle_error(anyhow!("Failed to extract pod utilization metrics. Make sure you have a metrics-server deployed on your cluster.")).await,
    };

    let mut app = self.app.lock().await;

    let data = make_qualifiers(&resources, &app.utilization_group_by, &[]);

    app.data.metrics.set_items(data);
  }

  pub async fn get_nodes(&self) {
    let lp = ListParams::default();
    let api_pods: Api<Pod> = Api::all(self.client.clone());
    let api_nodes: Api<Node> = Api::all(self.client.clone());

    match api_nodes.list(&lp).await {
      Ok(node_list) => {
        self.get_node_metrics().await;

        let pods_list = match api_pods.list(&lp).await {
          Ok(list) => list,
          Err(_) => ObjectList {
            metadata: ListMeta::default(),
            items: vec![],
          },
        };

        let mut app = self.app.lock().await;

        let items = node_list
          .iter()
          .map(|node| KubeNode::from_api_with_pods(node, &pods_list, &mut app))
          .collect::<Vec<_>>();

        app.data.nodes.set_items(items);
      }
      Err(e) => {
        self
          .handle_error(anyhow!("Failed to get nodes. {:?}", e))
          .await;
      }
    }
  }

  pub async fn get_namespaces(&self) {
    let api: Api<Namespace> = Api::all(self.client.clone());

    let lp = ListParams::default();
    match api.list(&lp).await {
      Ok(ns_list) => {
        let items = ns_list.into_iter().map(KubeNs::from).collect::<Vec<_>>();
        let mut app = self.app.lock().await;
        app.data.namespaces.set_items(items);
      }
      Err(e) => {
        self
          .handle_error(anyhow!("Failed to get namespaces. {:?}", e))
          .await;
      }
    }
  }

  pub async fn get_pods(&self) {
    let items: Vec<KubePod> = self.get_namespaced_resources(Pod::into).await;

    let mut app = self.app.lock().await;
    if app.data.selected.pod.is_some() {
      let containers = &items.iter().find_map(|pod| {
        if pod.name == app.data.selected.pod.clone().unwrap() {
          Some(&pod.containers)
        } else {
          None
        }
      });
      if containers.is_some() {
        app.data.containers.set_items(containers.unwrap().clone());
      }
    }
    app.data.pods.set_items(items);
  }

  pub async fn get_services(&self) {
    let items: Vec<KubeSvc> = self.get_namespaced_resources(Service::into).await;

    let mut app = self.app.lock().await;
    app.data.services.set_items(items);
  }

  pub async fn get_config_maps(&self) {
    let items: Vec<KubeConfigMap> = self.get_namespaced_resources(ConfigMap::into).await;

    let mut app = self.app.lock().await;
    app.data.config_maps.set_items(items);
  }

  pub async fn get_stateful_sets(&self) {
    let items: Vec<KubeStatefulSet> = self.get_namespaced_resources(StatefulSet::into).await;

    let mut app = self.app.lock().await;
    app.data.stateful_sets.set_items(items);
  }

  pub async fn get_replica_sets(&self) {
    let items: Vec<KubeReplicaSet> = self.get_namespaced_resources(ReplicaSet::into).await;

    let mut app = self.app.lock().await;
    app.data.replica_sets.set_items(items);
  }

  pub async fn get_jobs(&self) {
    let items: Vec<KubeJob> = self.get_namespaced_resources(Job::into).await;

    let mut app = self.app.lock().await;
    app.data.jobs.set_items(items);
  }

  pub async fn get_cron_jobs(&self) {
    let items: Vec<KubeCronJob> = self.get_namespaced_resources(CronJob::into).await;

    let mut app = self.app.lock().await;
    app.data.cronjobs.set_items(items);
  }

  pub async fn get_secrets(&self) {
    let items: Vec<KubeSecret> = self.get_namespaced_resources(Secret::into).await;

    let mut app = self.app.lock().await;
    app.data.secrets.set_items(items);
  }

  pub async fn get_replication_controllers(&self) {
    let items: Vec<KubeReplicationController> = self
      .get_namespaced_resources(ReplicationController::into)
      .await;

    let mut app = self.app.lock().await;
    app.data.rpl_ctrls.set_items(items);
  }

  pub async fn get_deployments(&self) {
    let items: Vec<KubeDeployment> = self.get_namespaced_resources(Deployment::into).await;

    let mut app = self.app.lock().await;
    app.data.deployments.set_items(items);
  }

  pub async fn get_daemon_sets_jobs(&self) {
    let items: Vec<KubeDaemonSet> = self.get_namespaced_resources(DaemonSet::into).await;

    let mut app = self.app.lock().await;
    app.data.daemon_sets.set_items(items);
  }

  pub async fn get_storage_classes(&self) {
    let items: Vec<KubeStorageClass> = self.get_resources(StorageClass::into).await;

    let mut app = self.app.lock().await;
    app.data.storage_classes.set_items(items);
  }

  pub async fn get_roles(&self) {
    let items: Vec<KubeRole> = self.get_namespaced_resources(Role::into).await;

    let mut app = self.app.lock().await;
    app.data.roles.set_items(items);
  }

  pub async fn get_role_bindings(&self) {
    let items: Vec<KubeRoleBinding> = self.get_namespaced_resources(RoleBinding::into).await;

    let mut app = self.app.lock().await;
    app.data.role_bindings.set_items(items);
  }

  pub async fn get_cluster_roles(&self) {
    let items: Vec<KubeClusterRole> = self.get_resources(ClusterRole::into).await;

    let mut app = self.app.lock().await;
    app.data.cluster_roles.set_items(items);
  }

  pub async fn get_cluster_role_binding(&self) {
    let items: Vec<KubeClusterRoleBinding> = self.get_resources(ClusterRoleBinding::into).await;

    let mut app = self.app.lock().await;
    app.data.cluster_role_bindings.set_items(items);
  }

  pub async fn get_ingress(&self) {
    let items: Vec<KubeIngress> = self.get_namespaced_resources(Ingress::into).await;

    let mut app = self.app.lock().await;
    app.data.ingress.set_items(items);
  }

  pub async fn get_pvcs(&self) {
    let items: Vec<KubePVC> = self
      .get_namespaced_resources(PersistentVolumeClaim::into)
      .await;

    let mut app = self.app.lock().await;
    app.data.pvcs.set_items(items);
  }

  pub async fn get_pvs(&self) {
    let items: Vec<KubePV> = self.get_resources(PersistentVolume::into).await;

    let mut app = self.app.lock().await;
    app.data.pvs.set_items(items);
  }

  pub async fn get_service_accounts(&self) {
    let items: Vec<KubeSvcAcct> = self.get_namespaced_resources(ServiceAccount::into).await;

    let mut app = self.app.lock().await;
    app.data.service_accounts.set_items(items);
  }

  /// calls the kubernetes API to list the given resource for either selected namespace or all namespaces
  async fn get_namespaced_resources<K: ApiResource, T, F>(&self, map_fn: F) -> Vec<T>
  where
    <K as ApiResource>::DynamicType: Default,
    K: kube::Resource<Scope = NamespaceResourceScope>,
    K: Clone + DeserializeOwned + fmt::Debug,
    F: Fn(K) -> T,
  {
    let api: Api<K> = self.get_namespaced_api().await;
    let lp = ListParams::default();
    match api.list(&lp).await {
      Ok(list) => list.into_iter().map(map_fn).collect::<Vec<_>>(),
      Err(e) => {
        self
          .handle_error(anyhow!(
            "Failed to get namespaced resource {}. {:?}",
            std::any::type_name::<T>(),
            e
          ))
          .await;
        vec![]
      }
    }
  }

  async fn get_resources<K: ApiResource, T, F>(&self, map_fn: F) -> Vec<T>
  where
    <K as ApiResource>::DynamicType: Default,
    K: Clone + DeserializeOwned + fmt::Debug,
    F: Fn(K) -> T,
  {
    let api: Api<K> = Api::all(self.client.clone());
    let lp = ListParams::default();
    match api.list(&lp).await {
      Ok(list) => list.into_iter().map(map_fn).collect::<Vec<_>>(),
      Err(e) => {
        self
          .handle_error(anyhow!(
            "Failed to get resource {}. {:?}",
            std::any::type_name::<T>(),
            e
          ))
          .await;
        vec![]
      }
    }
  }

  async fn get_namespaced_api<K: ApiResource>(&self) -> Api<K>
  where
    <K as ApiResource>::DynamicType: Default,
    K: kube::Resource<Scope = NamespaceResourceScope>,
  {
    let app = self.app.lock().await;
    match &app.data.selected.ns {
      Some(ns) => Api::namespaced(self.client.clone(), ns),
      None => Api::all(self.client.clone()),
    }
  }
}
