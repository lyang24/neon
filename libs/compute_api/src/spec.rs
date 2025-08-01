//! The ComputeSpec contains all the information needed to start up
//! the right version of PostgreSQL, and connect it to the storage nodes.
//! It can be passed as part of the `config.json`, or the control plane can
//! provide it by calling the compute_ctl's `/compute_ctl` endpoint, or
//! compute_ctl can fetch it by calling the control plane's API.
use std::collections::HashMap;
use std::fmt::Display;

use anyhow::anyhow;
use indexmap::IndexMap;
use regex::Regex;
use remote_storage::RemotePath;
use serde::{Deserialize, Serialize};
use url::Url;
use utils::id::{TenantId, TimelineId};
use utils::lsn::Lsn;

use crate::responses::TlsConfig;

/// String type alias representing Postgres identifier and
/// intended to be used for DB / role names.
pub type PgIdent = String;

/// String type alias representing Postgres extension version
pub type ExtVersion = String;

fn default_reconfigure_concurrency() -> usize {
    1
}

/// Cluster spec or configuration represented as an optional number of
/// delta operations + final cluster state description.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ComputeSpec {
    pub format_version: f32,

    // The control plane also includes a 'timestamp' field in the JSON document,
    // but we don't use it for anything. Serde will ignore missing fields when
    // deserializing it.
    pub operation_uuid: Option<String>,

    /// Compute features to enable. These feature flags are provided, when we
    /// know all the details about client's compute, so they cannot be used
    /// to change `Empty` compute behavior.
    #[serde(default)]
    pub features: Vec<ComputeFeature>,

    /// If compute_ctl was passed `--resize-swap-on-bind`, a value of `Some(_)` instructs
    /// compute_ctl to `/neonvm/bin/resize-swap` with the given size, when the spec is first
    /// received.
    ///
    /// Both this field and `--resize-swap-on-bind` are required, so that the control plane's
    /// spec generation doesn't need to be aware of the actual compute it's running on, while
    /// guaranteeing gradual rollout of swap. Otherwise, without `--resize-swap-on-bind`, we could
    /// end up trying to resize swap in VMs without it -- or end up *not* resizing swap, thus
    /// giving every VM much more swap than it should have (32GiB).
    ///
    /// Eventually we may remove `--resize-swap-on-bind` and exclusively use `swap_size_bytes` for
    /// enabling the swap resizing behavior once rollout is complete.
    ///
    /// See neondatabase/cloud#12047 for more.
    #[serde(default)]
    pub swap_size_bytes: Option<u64>,

    /// If compute_ctl was passed `--set-disk-quota-for-fs`, a value of `Some(_)` instructs
    /// compute_ctl to run `/neonvm/bin/set-disk-quota` with the given size and fs, when the
    /// spec is first received.
    ///
    /// Both this field and `--set-disk-quota-for-fs` are required, so that the control plane's
    /// spec generation doesn't need to be aware of the actual compute it's running on, while
    /// guaranteeing gradual rollout of disk quota.
    #[serde(default)]
    pub disk_quota_bytes: Option<u64>,

    /// Disables the vm-monitor behavior that resizes LFC on upscale/downscale, instead relying on
    /// the initial size of LFC.
    ///
    /// This is intended for use when the LFC size is being overridden from the default but
    /// autoscaling is still enabled, and we don't want the vm-monitor to interfere with the custom
    /// LFC sizing.
    #[serde(default)]
    pub disable_lfc_resizing: Option<bool>,

    /// Expected cluster state at the end of transition process.
    pub cluster: Cluster,
    pub delta_operations: Option<Vec<DeltaOp>>,

    /// An optional hint that can be passed to speed up startup time if we know
    /// that no pg catalog mutations (like role creation, database creation,
    /// extension creation) need to be done on the actual database to start.
    #[serde(default)] // Default false
    pub skip_pg_catalog_updates: bool,

    // Information needed to connect to the storage layer.
    //
    // `tenant_id`, `timeline_id` and `pageserver_connstring` are always needed.
    //
    // Depending on `mode`, this can be a primary read-write node, a read-only
    // replica, or a read-only node pinned at an older LSN.
    // `safekeeper_connstrings` must be set for a primary.
    //
    // For backwards compatibility, the control plane may leave out all of
    // these, and instead set the "neon.tenant_id", "neon.timeline_id",
    // etc. GUCs in cluster.settings. TODO: Once the control plane has been
    // updated to fill these fields, we can make these non optional.
    pub tenant_id: Option<TenantId>,
    pub timeline_id: Option<TimelineId>,
    pub pageserver_connstring: Option<String>,

    // More neon ids that we expose to the compute_ctl
    // and to postgres as neon extension GUCs.
    pub project_id: Option<String>,
    pub branch_id: Option<String>,
    pub endpoint_id: Option<String>,

    /// Safekeeper membership config generation. It is put in
    /// neon.safekeepers GUC and serves two purposes:
    /// 1) Non zero value forces walproposer to use membership configurations.
    /// 2) If walproposer wants to update list of safekeepers to connect to
    ///    taking them from some safekeeper mconf, it should check what value
    ///    is newer by comparing the generation.
    ///
    /// Note: it could be SafekeeperGeneration, but this needs linking
    /// compute_ctl with postgres_ffi.
    #[serde(default)]
    pub safekeepers_generation: Option<u32>,
    #[serde(default)]
    pub safekeeper_connstrings: Vec<String>,

    #[serde(default)]
    pub mode: ComputeMode,

    /// If set, 'storage_auth_token' is used as the password to authenticate to
    /// the pageserver and safekeepers.
    pub storage_auth_token: Option<String>,

    // information about available remote extensions
    pub remote_extensions: Option<RemoteExtSpec>,

    pub pgbouncer_settings: Option<IndexMap<String, String>>,

    // Stripe size for pageserver sharding, in pages
    #[serde(default)]
    pub shard_stripe_size: Option<usize>,

    /// Local Proxy configuration used for JWT authentication
    #[serde(default)]
    pub local_proxy_config: Option<LocalProxySpec>,

    /// Number of concurrent connections during the parallel RunInEachDatabase
    /// phase of the apply config process.
    ///
    /// We need a higher concurrency during reconfiguration in case of many DBs,
    /// but instance is already running and used by client. We can easily get out of
    /// `max_connections` limit, and the current code won't handle that.
    ///
    /// Default is 1, but also allow control plane to override this value for specific
    /// projects. It's also recommended to bump `superuser_reserved_connections` +=
    /// `reconfigure_concurrency` for such projects to ensure that we always have
    /// enough spare connections for reconfiguration process to succeed.
    #[serde(default = "default_reconfigure_concurrency")]
    pub reconfigure_concurrency: usize,

    /// If set to true, the compute_ctl will drop all subscriptions before starting the
    /// compute. This is needed when we start an endpoint on a branch, so that child
    /// would not compete with parent branch subscriptions
    /// over the same replication content from publisher.
    #[serde(default)] // Default false
    pub drop_subscriptions_before_start: bool,

    /// Log level for compute audit logging
    #[serde(default)]
    pub audit_log_level: ComputeAudit,

    /// Hostname and the port of the otel collector. Leave empty to disable Postgres logs forwarding.
    /// Example: config-shy-breeze-123-collector-monitoring.neon-telemetry.svc.cluster.local:10514
    pub logs_export_host: Option<String>,

    /// Address of endpoint storage service
    pub endpoint_storage_addr: Option<String>,
    /// JWT for authorizing requests to endpoint storage service
    pub endpoint_storage_token: Option<String>,

    #[serde(default)]
    /// Download LFC state from endpoint storage and pass it to Postgres on compute startup
    pub autoprewarm: bool,

    #[serde(default)]
    /// Upload LFC state to endpoint storage periodically. Default value (None) means "don't upload"
    pub offload_lfc_interval_seconds: Option<std::num::NonZeroU64>,

    /// Suspend timeout in seconds.
    ///
    /// We use this value to derive other values, such as the installed extensions metric.
    pub suspend_timeout_seconds: i64,
}

/// Feature flag to signal `compute_ctl` to enable certain experimental functionality.
#[derive(Serialize, Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComputeFeature {
    // XXX: Add more feature flags here.
    /// Enable the experimental activity monitor logic, which uses `pg_stat_database` to
    /// track short-lived connections as user activity.
    ActivityMonitorExperimental,

    /// Enable TLS functionality.
    TlsExperimental,

    /// This is a special feature flag that is used to represent unknown feature flags.
    /// Basically all unknown to enum flags are represented as this one. See unit test
    /// `parse_unknown_features()` for more details.
    #[serde(other)]
    UnknownFeature,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RemoteExtSpec {
    pub public_extensions: Option<Vec<String>>,
    pub custom_extensions: Option<Vec<String>>,
    pub library_index: HashMap<String, String>,
    pub extension_data: HashMap<String, ExtensionData>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExtensionData {
    pub control_data: HashMap<String, String>,
    pub archive_path: String,
}

impl RemoteExtSpec {
    pub fn get_ext(
        &self,
        ext_name: &str,
        is_library: bool,
        build_tag: &str,
        pg_major_version: &str,
    ) -> anyhow::Result<(String, RemotePath)> {
        let mut real_ext_name = ext_name;
        if is_library {
            // sometimes library names might have a suffix like
            // library.so or library.so.3. We strip this off
            // because library_index is based on the name without the file extension
            let strip_lib_suffix = Regex::new(r"\.so.*").unwrap();
            let lib_raw_name = strip_lib_suffix.replace(real_ext_name, "").to_string();

            real_ext_name = self
                .library_index
                .get(&lib_raw_name)
                .ok_or(anyhow::anyhow!("library {} is not found", lib_raw_name))?;
        }

        // Check if extension is present in public or custom.
        // If not, then it is not allowed to be used by this compute.
        if !self
            .public_extensions
            .as_ref()
            .is_some_and(|exts| exts.iter().any(|e| e == real_ext_name))
            && !self
                .custom_extensions
                .as_ref()
                .is_some_and(|exts| exts.iter().any(|e| e == real_ext_name))
        {
            return Err(anyhow::anyhow!("extension {} is not found", real_ext_name));
        }

        match self.extension_data.get(real_ext_name) {
            Some(_ext_data) => Ok((
                real_ext_name.to_string(),
                Self::build_remote_path(build_tag, pg_major_version, real_ext_name)?,
            )),
            None => Err(anyhow::anyhow!(
                "real_ext_name {} is not found",
                real_ext_name
            )),
        }
    }

    /// Get the architecture-specific portion of the remote extension path. We
    /// use the Go naming convention due to Kubernetes.
    fn get_arch() -> &'static str {
        match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            arch => arch,
        }
    }

    /// Build a [`RemotePath`] for an extension.
    fn build_remote_path(
        build_tag: &str,
        pg_major_version: &str,
        ext_name: &str,
    ) -> anyhow::Result<RemotePath> {
        let arch = Self::get_arch();

        // Construct the path to the extension archive
        // BUILD_TAG/PG_MAJOR_VERSION/extensions/EXTENSION_NAME.tar.zst
        //
        // Keep it in sync with path generation in
        // https://github.com/neondatabase/build-custom-extensions/tree/main
        RemotePath::from_string(&format!(
            "{build_tag}/{arch}/{pg_major_version}/extensions/{ext_name}.tar.zst"
        ))
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub enum ComputeMode {
    /// A read-write node
    #[default]
    Primary,
    /// A read-only node, pinned at a particular LSN
    Static(Lsn),
    /// A read-only node that follows the tip of the branch in hot standby mode
    ///
    /// Future versions may want to distinguish between replicas with hot standby
    /// feedback and other kinds of replication configurations.
    Replica,
}

impl ComputeMode {
    /// Convert the compute mode to a string that can be used to identify the type of compute,
    /// which means that if it's a static compute, the LSN will not be included.
    pub fn to_type_str(&self) -> &'static str {
        match self {
            ComputeMode::Primary => "primary",
            ComputeMode::Static(_) => "static",
            ComputeMode::Replica => "replica",
        }
    }
}

/// Log level for audit logging
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub enum ComputeAudit {
    #[default]
    Disabled,
    // Deprecated, use Base instead
    Log,
    // (pgaudit.log = 'ddl', pgaudit.log_parameter='off')
    // logged to the standard postgresql log stream
    Base,
    // Deprecated, use Full or Extended instead
    Hipaa,
    // (pgaudit.log = 'all, -misc', pgaudit.log_parameter='off')
    // logged to separate files collected by rsyslog
    // into dedicated log storage with strict access
    Extended,
    // (pgaudit.log='all', pgaudit.log_parameter='on'),
    // logged to separate files collected by rsyslog
    // into dedicated log storage with strict access.
    Full,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Cluster {
    pub cluster_id: Option<String>,
    pub name: Option<String>,
    pub state: Option<String>,
    pub roles: Vec<Role>,
    pub databases: Vec<Database>,

    /// Desired contents of 'postgresql.conf' file. (The 'compute_ctl'
    /// tool may add additional settings to the final file.)
    pub postgresql_conf: Option<String>,

    /// Additional settings that will be appended to the 'postgresql.conf' file.
    pub settings: GenericOptions,
}

/// Single cluster state changing operation that could not be represented as
/// a static `Cluster` structure. For example:
/// - DROP DATABASE
/// - DROP ROLE
/// - ALTER ROLE name RENAME TO new_name
/// - ALTER DATABASE name RENAME TO new_name
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeltaOp {
    pub action: String,
    pub name: PgIdent,
    pub new_name: Option<PgIdent>,
}

/// Rust representation of Postgres role info with only those fields
/// that matter for us.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Role {
    pub name: PgIdent,
    pub encrypted_password: Option<String>,
    pub options: GenericOptions,
}

/// Rust representation of Postgres database info with only those fields
/// that matter for us.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Database {
    pub name: PgIdent,
    pub owner: PgIdent,
    pub options: GenericOptions,
    // These are derived flags, not present in the spec file.
    // They are never set by the control plane.
    #[serde(skip_deserializing, default)]
    pub restrict_conn: bool,
    #[serde(skip_deserializing, default)]
    pub invalid: bool,
}

/// Common type representing both SQL statement params with or without value,
/// like `LOGIN` or `OWNER username` in the `CREATE/ALTER ROLE`, and config
/// options like `wal_level = logical`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct GenericOption {
    pub name: String,
    pub value: Option<String>,
    pub vartype: String,
}

/// Optional collection of `GenericOption`'s. Type alias allows us to
/// declare a `trait` on it.
pub type GenericOptions = Option<Vec<GenericOption>>;

/// Configured the local_proxy application with the relevant JWKS and roles it should
/// use for authorizing connect requests using JWT.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LocalProxySpec {
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jwks: Option<Vec<JwksSettings>>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JwksSettings {
    pub id: String,
    pub role_names: Vec<String>,
    pub jwks_url: String,
    pub provider_name: String,
    pub jwt_audience: Option<String>,
}

/// Protocol used to connect to a Pageserver. Parsed from the connstring scheme.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PageserverProtocol {
    /// The original protocol based on libpq and COPY. Uses postgresql:// or postgres:// scheme.
    #[default]
    Libpq,
    /// A newer, gRPC-based protocol. Uses grpc:// scheme.
    Grpc,
}

impl PageserverProtocol {
    /// Parses the protocol from a connstring scheme. Defaults to Libpq if no scheme is given.
    /// Errors if the connstring is an invalid URL.
    pub fn from_connstring(connstring: &str) -> anyhow::Result<Self> {
        let scheme = match Url::parse(connstring) {
            Ok(url) => url.scheme().to_lowercase(),
            Err(url::ParseError::RelativeUrlWithoutBase) => return Ok(Self::default()),
            Err(err) => return Err(anyhow!("invalid connstring URL: {err}")),
        };
        match scheme.as_str() {
            "postgresql" | "postgres" => Ok(Self::Libpq),
            "grpc" => Ok(Self::Grpc),
            scheme => Err(anyhow!("invalid protocol scheme: {scheme}")),
        }
    }

    /// Returns the URL scheme for the protocol, for use in connstrings.
    pub fn scheme(&self) -> &'static str {
        match self {
            Self::Libpq => "postgresql",
            Self::Grpc => "grpc",
        }
    }
}

impl Display for PageserverProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.scheme())
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use super::*;

    #[test]
    fn allow_installing_remote_extensions() {
        let rspec: RemoteExtSpec = serde_json::from_value(serde_json::json!({
            "public_extensions": null,
            "custom_extensions": null,
            "library_index": {},
            "extension_data": {},
        }))
        .unwrap();

        rspec
            .get_ext("ext", false, "latest", "v17")
            .expect_err("Extension should not be found");

        let rspec: RemoteExtSpec = serde_json::from_value(serde_json::json!({
            "public_extensions": [],
            "custom_extensions": null,
            "library_index": {},
            "extension_data": {},
        }))
        .unwrap();

        rspec
            .get_ext("ext", false, "latest", "v17")
            .expect_err("Extension should not be found");

        let rspec: RemoteExtSpec = serde_json::from_value(serde_json::json!({
            "public_extensions": [],
            "custom_extensions": [],
            "library_index": {
                "ext": "ext"
            },
            "extension_data": {
                "ext": {
                    "control_data": {
                        "ext.control": ""
                    },
                    "archive_path": ""
                }
            },
        }))
        .unwrap();

        rspec
            .get_ext("ext", false, "latest", "v17")
            .expect_err("Extension should not be found");

        let rspec: RemoteExtSpec = serde_json::from_value(serde_json::json!({
            "public_extensions": [],
            "custom_extensions": ["ext"],
            "library_index": {
                "ext": "ext"
            },
            "extension_data": {
                "ext": {
                    "control_data": {
                        "ext.control": ""
                    },
                    "archive_path": ""
                }
            },
        }))
        .unwrap();

        rspec
            .get_ext("ext", false, "latest", "v17")
            .expect("Extension should be found");

        let rspec: RemoteExtSpec = serde_json::from_value(serde_json::json!({
            "public_extensions": ["ext"],
            "custom_extensions": [],
            "library_index": {
                "extlib": "ext",
            },
            "extension_data": {
                "ext": {
                    "control_data": {
                        "ext.control": ""
                    },
                    "archive_path": ""
                }
            },
        }))
        .unwrap();

        rspec
            .get_ext("ext", false, "latest", "v17")
            .expect("Extension should be found");

        // test library index for the case when library name
        // doesn't match the extension name
        rspec
            .get_ext("extlib", true, "latest", "v17")
            .expect("Library should be found");
    }

    #[test]
    fn remote_extension_path() {
        let rspec: RemoteExtSpec = serde_json::from_value(serde_json::json!({
            "public_extensions": ["ext"],
            "custom_extensions": [],
            "library_index": {
                "extlib": "ext",
            },
            "extension_data": {
                "ext": {
                    "control_data": {
                        "ext.control": ""
                    },
                    "archive_path": ""
                }
            },
        }))
        .unwrap();

        let (_ext_name, ext_path) = rspec
            .get_ext("ext", false, "latest", "v17")
            .expect("Extension should be found");
        // Starting with a forward slash would have consequences for the
        // Url::join() that occurs when downloading a remote extension.
        assert!(!ext_path.to_string().starts_with("/"));
        assert_eq!(
            ext_path,
            RemoteExtSpec::build_remote_path("latest", "v17", "ext").unwrap()
        );
    }

    #[test]
    fn parse_spec_file() {
        let file = File::open("tests/cluster_spec.json").unwrap();
        let spec: ComputeSpec = serde_json::from_reader(file).unwrap();

        // Features list defaults to empty vector.
        assert!(spec.features.is_empty());

        // Reconfigure concurrency defaults to 1.
        assert_eq!(spec.reconfigure_concurrency, 1);
    }

    #[test]
    fn parse_unknown_fields() {
        // Forward compatibility test
        let file = File::open("tests/cluster_spec.json").unwrap();
        let mut json: serde_json::Value = serde_json::from_reader(file).unwrap();
        let ob = json.as_object_mut().unwrap();
        ob.insert("unknown_field_123123123".into(), "hello".into());
        let _spec: ComputeSpec = serde_json::from_value(json).unwrap();
    }

    #[test]
    fn parse_unknown_features() {
        // Test that unknown feature flags do not cause any errors.
        let file = File::open("tests/cluster_spec.json").unwrap();
        let mut json: serde_json::Value = serde_json::from_reader(file).unwrap();
        let ob = json.as_object_mut().unwrap();

        // Add unknown feature flags.
        let features = vec!["foo_bar_feature", "baz_feature"];
        ob.insert("features".into(), features.into());

        let spec: ComputeSpec = serde_json::from_value(json).unwrap();

        assert!(spec.features.len() == 2);
        assert!(spec.features.contains(&ComputeFeature::UnknownFeature));
        assert_eq!(spec.features, vec![ComputeFeature::UnknownFeature; 2]);
    }

    #[test]
    fn parse_known_features() {
        // Test that we can properly parse known feature flags.
        let file = File::open("tests/cluster_spec.json").unwrap();
        let mut json: serde_json::Value = serde_json::from_reader(file).unwrap();
        let ob = json.as_object_mut().unwrap();

        // Add known feature flags.
        let features = vec!["activity_monitor_experimental"];
        ob.insert("features".into(), features.into());

        let spec: ComputeSpec = serde_json::from_value(json).unwrap();

        assert_eq!(
            spec.features,
            vec![ComputeFeature::ActivityMonitorExperimental]
        );
    }
}
