Talos Linux supports ETCD Encryption via KMS.
- https://docs.siderolabs.com/talos/v1.14/reference/configuration/kubernetes/kubeetcdencryptionconfig#kubeetcdencryptionconfig
- https://oneuptime.com/blog/post/2026-03-03-set-up-etcd-encryption-in-talos-linux/view
- https://oneuptime.com/blog/post/2026-03-03-configure-kubernetes-secrets-encryption-on-talos/view

Talos Linux supports KMS Disk Encryption
- https://docs.siderolabs.com/talos/v1.8/configure-your-talos-cluster/storage-and-disk-management/disk-encryption
- https://oneuptime.com/blog/post/2026-03-03-use-kms-based-disk-encryption-in-talos-linux/view


Kubernetes Secrets Encryption with KMS:
- https://kubernetes.io/docs/tasks/administer-cluster/kms-provider/
- https://kubernetes.io/docs/tasks/administer-cluster/encrypt-data/#verifying-that-data-is-encrypted

I want to add a KMS Component to enable etcd encryption of secrets on the cluster.  I would like the controller to installiing the KMS component, encrypting any unencrypted secrets that are in plain text at time of install, and configuring the cluster to disable plaintext secrets.

I would like to use the following KMS engines:
- Openstack Barbican:
  - https://raw.githubusercontent.com/kubernetes/cloud-provider-openstack/refs/heads/master/docs/barbican-kms-plugin/using-barbican-kms-plugin.md
- Azure Key Management:
  - https://raw.githubusercontent.com/kubernetes/cloud-provider-azure/refs/heads/master/docs/azure-kms-plugin/using-azure-kms-plugin.md
- Amazon Web Services KMS:
  - https://raw.githubusercontent.com/kubernetes/cloud-provider-aws/refs/heads/master/docs/aws-kms-plugin/using-aws-kms-plugin.md
- Google Cloud KMS:
  - https://raw.githubusercontent.com/kubernetes/cloud-provider-gcp/refs/heads/master/docs/gcp-kms-plugin/using-gcp-kms-plugin.md
- Oracle Cloud Infrastructure KMS:
  - https://raw.githubusercontent.com/kubernetes/cloud-provider-oci/refs/heads/master/docs/oci-kms-plugin/using-oci-kms-plugin.md