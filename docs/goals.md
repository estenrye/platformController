I want to create a platform operator that interacts with Kubernetes using Rust.  This operator should be able to be installed on a Kubernetes cluster without a CNI or CSI and install all platform components necessary for the cluster to function correctly.

The operator must:
  - Be completely declarative.
  - Be installable on a Kubernetes cluster without a CNI or CSI.
  - Install all platform components necessary for the cluster to function correctly.
  - Be extensible through configuration such that it can operate in a cloud agnostic manner across multiple cloud underlays.
  - Manage the lifecycle of platform components, ensuring they are correctly deployed, updated, and removed as needed.
  - Be observable by providing detailed insights into its internal state and behavior through logging, metrics, and events, to facilitate monitoring and troubleshooting.
  - Handle error recovery and ensure the platform remains in a consistent state, even in the face of failures or misconfigurations.
  - Be secure, ensuring that all interactions with the Kubernetes API and platform components are properly authenticated and authorized, and that sensitive data is handled safely.
  - Be performant, minimizing resource usage and ensuring that it can scale effectively with the size of the cluster and the number of platform components being managed.
  - Be resilient, capable of recovering gracefully from failures and maintaining the desired state of the platform components even under adverse conditions.
  - Be self-healing, automatically detecting and correcting issues with platform components to minimize downtime and manual intervention.
  - Be highly available, ensuring that it continues to operate correctly even if some of its instances fail or become unreachable.
  - Be maintainable, with clear documentation, modular code structure, and well-defined interfaces to facilitate ongoing development and maintenance.
  - Be testable, with comprehensive unit and integration tests to ensure correctness and reliability of the operator's functionality.
  - Be configurable, allowing cluster administrators to customize its behavior and the deployment of platform components according to the specific needs of their environment.
  - Be compatible with existing Kubernetes tools and workflows, ensuring that it integrates seamlessly into the cluster administrator's operational practices.
  - Be upgradeable, allowing for smooth and reliable updates to the operator itself and the platform components it manages, without causing disruption to the cluster.

# Cloud Underlay Specification

| Name | Data Type     | Description | Validation Policy | Required |
|------|---------------|-------------|------------------|----------|
| profile | enum(string) | The profile of the cloud underlay being used. | must be one of `["aws-platform", "oci-platform", "gke-platform", "azure-platform", "rackspace-spot-platform", "self-hosted-talos", "self-hosted-k3s", "self-hosted-generic"]` | true |
| platform | Platform | The platform specification for the cloud underlay. | must conform to the Platform specification defined below. | true |
| externalServices | ExternalServicesBinding | The configuration for external services used by the cloud underlay. | must conform to the ExternalServicesBinding specification defined below. | false |

## Platform Specification

| Name | Data Type     | Description | Validation Policy | Required |
|------|---------------|-------------|------------------|----------|
| kind | enum(string)  | This enumeration defines the type of Kubernetes cluster to which the platform is being installed.  Keying off this value allows the operator to tailor its behavior to support specific cluster types. | must be one of `["talos-linux", "rackspace-spot", "aws-eks", "gcp-gke", "azure-aks", "oci-oke", "k3s", "generic"]` | true |
| kubernetesVersionConstraint | string | A constraint specifying the acceptable Kubernetes versions for the platform. | must be a valid semantic version constraint (e.g., `>=1.20.0 <1.25.0`) | true |
| region | string | The geographical region in which the platform is being deployed. | must be a valid cloud provider region identifier (e.g., `us-west-1`, `europe-west1`) | false |
| compartment | string | The compartment or project within the cloud provider where the platform is being deployed. | must be a valid cloud provider compartment identifier (e.g., `default`, `my-project`) | false |
| airgapped | boolean | Indicates whether the platform is deployed in an airgapped environment. | must be a boolean value (`true` or `false`) | false |

## External Services Specification

| Name | Data Type     | Description | Validation Policy | Required |
|------|---------------|-------------|------------------|----------|
| cni | CniBinding | The configuration for the CNI plugin within the Kubernetes cluster. | must conform to the CniBinding specification defined below. | false |

### CNI Binding Specification

| Name | Data Type     | Description | Validation Policy | Required |
|------|---------------|-------------|------------------|----------|
| provider | enum(string) | The CNI plugin to be used for networking within the Kubernetes cluster. | must be one of `["calico", "aws-vpc-cni", "oci-vcn-native", "azure-cni", "google-cni", "rspot-calico-iptables", "rspot-cillium", "rspot-byocni-calico"]` | true |
| calico | CalicoBinding | The configuration specific to the Calico CNI plugin. | must conform to the CalicoBinding specification defined below. | false |
| awsVpcCni | AwsVpcCniBinding | The configuration specific to the AWS VPC CNI plugin. | must conform to the AwsVpcCniBinding specification defined below. | false |
| ociVcnNative | OciVcnNativeBinding | The configuration specific to the OCI VCN Native CNI plugin. | must conform to the OciVcnNativeBinding specification defined below. | false |
| azureCni | AzureCniBinding | The configuration specific to the Azure CNI plugin. | must conform to the AzureCniBinding specification defined below. | false |
| googleCni | GoogleCniBinding | The configuration specific to the Google CNI plugin. | must conform to the GoogleCniBinding specification defined below. | false |
| rspotCalicoIptables | RspotCalicoIptablesBinding | The configuration specific to the Rackspace Spot Calico IPTables CNI plugin. | must conform to the RspotCalicoIptablesBinding specification defined below. | false |
| rspotCillium | RspotCilliumBinding | The configuration specific to the Rackspace Spot Cillium CNI plugin. | must conform to the RspotCilliumBinding specification defined below. | false |
| rspotByocniCalico | RspotByocniCalicoBinding | The configuration specific to the Rackspace Spot BYOCNI Calico CNI plugin. | must conform to the RspotByocniCalicoBinding specification defined below. | false |

### Calico Binding Specification

| Name | Data Type     | Description | Validation Policy | Required |
|------|---------------|-------------|------------------|----------|
| component | ComponentSpec | The specification for the Calico component within the CNI plugin. | must conform to the ComponentSpec specification defined below. | false |
| bgpEnabled | boolean | Indicates whether BGP is enabled for the Calico CNI plugin. | must be a boolean value (`true` or `false`). | false | `false` |
| apiServerEnabled | boolean | Indicates whether the API server is enabled for the Calico CNI plugin. | must be a boolean value (`true` or `false`). | false | `false` |
| ipPools | CalicoIpPoolSpec[] | The list of IP pool specifications for the Calico CNI plugin. | must conform to the CalicoIpPoolSpec specification defined below. | false | `[]` |

#### Calico IP Pool Specification

| Name | Data Type     | Description | Validation Policy | Required | Default |
|------|---------------|-------------|------------------|----------|---------|
| cidr | string | The CIDR block for the IP pool. | must be a valid IPv4 or IPv6 CIDR notation. | true | `10.0.0.0/16` |
| name | string | The name of the IP pool. | must be a non-empty string. | true | `default` |
| encapsulation | enum(string) | The encapsulation mode for the IP pool. | must be one of `IPIP`, `VXLAN`, or `None`. | false | `None` |
| natOutgoing | boolean | Indicates whether NAT outgoing is enabled for the IP pool. | must be a boolean value (`true` or `false`). | false | `true` |
| blockSize | integer | The size of the IP blocks within the IP pool. | must be a positive integer. | false | `112` |
| nodeSelector | string | The node selector for the IP pool. | must be a valid Kubernetes label selector. | false | `"all()"` |
| nodeAddressAutodetectionV6Cidrs | string[] | The list of IPv6 CIDRs for node address autodetection. | must be a list of valid IPv6 CIDR notations. | false | `[]` |


## Component Spec

| Name | Data Type     | Description | Validation Policy | Required | Default |
|------|---------------|-------------|------------------|----------| ---- |
| enabled | boolean | Indicates whether the component is enabled. | must be a boolean value (`true` or `false`). | false | `true` |
| images | ImageTagSpec[] | A list of image tag specifications for the component. | must conform to the ImageTagSpec specification defined below. | false | `[]` |
| chart | ChartRefSpec | The Helm chart reference for the component. | must conform to the ChartRefSpec specification defined below. | false | `nil` |
| helmValues | apiextensionsv1.JSON | HelmValues is free-form passthrough merged into the component's values subtree. The escape hatch for anything the typed fields don't reach. | must be a valid JSON object. | false | `{}` |
| strategicMergePatches | apiextensionsv1.JSON[] | StrategicMergePatches are applied to rendered manifests post-templating.  Deliberately last-resort: every use is a gap in the typed API above and should be tracked as such. | must be a valid JSON object. | false | `[]` |

## Image Tag Spec

| Name | Data Type     | Description | Validation Policy | Required | Default |
|------|---------------|-------------|------------------|----------|---------|
| kind | string | The kind of the Kubernetes resource the image tag is associated with (e.g., `Deployment`, `DaemonSet`). | must be a valid Kubernetes resource kind. | false | `Deployment` |
| group | string | The group of the Kubernetes resource the image tag is associated with (e.g., `apps`, `batch`). | must be a valid Kubernetes API group. | false | `apps` |
| version | string | The version of the Kubernetes resource the image tag is associated with (e.g., `v1`). | must be a valid Kubernetes API version. | false | `v1` |
| name | string | The name of the Kubernetes resource the image tag is associated with (e.g., `my-deployment`). | must be a valid Kubernetes resource name. | false | `my-deployment` |
| namespace | string | The namespace of the Kubernetes resource the image tag is associated with (e.g., `default`). | must be a valid Kubernetes namespace. | false | `default` |
| repository | string | The image repository for the component. | must be a valid container image repository URL. | true | `docker.io/library` |
| image | string | The image name for the component. | must be a valid container image name. | true | `my-image` |
| tag | string | The image tag for the component. | must be a valid semantic version string (e.g., `v1.2.3`). | true | `latest` |
| digest | string | The image digest for the component. | must be a valid container image digest. | false | `""` |
| pullSecretRef | corev1.LocalObjectReference | A reference to the pull secret for the image. | must be a valid Kubernetes local object reference. | false | `nil` |

## Chart Ref Spec

| Name | Data Type     | Description | Validation Policy | Required | Default |
|------|---------------|-------------|------------------|----------|---------|
| name | string | The name of the Helm chart. | must be a valid Helm chart name. | true | `my-chart` |
| version | string | The version of the Helm chart. | must be a valid semantic version string (e.g., `v1.2.3`). | true | `v1.0.0` |
| repository | string | The repository URL of the Helm chart. | must be a valid URL. | true | `https://charts.example.com` |
| pullSecretRef | corev1.LocalObjectReference | A reference to the pull secret for the Helm chart. | must be a valid Kubernetes local object reference. | false | `nil` |