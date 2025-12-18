use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use clap::{Parser, Subcommand};
use color_eyre::eyre::{Result, bail, eyre};
use k256::{SecretKey, elliptic_curve::sec1::ToEncodedPoint};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ultramarine_types::{
    genesis::Genesis as ConsensusGenesis,
    signing::PrivateKey as ConsensusPrivateKey,
    validator_set::{Validator, ValidatorSet},
};

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Validate `infra/manifests/<net>.yaml`.
    Validate {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long, default_value_t = false)]
        allow_unsafe_failure_domains: bool,
    },
    /// Generate lockfile + bundle outputs under `infra/networks/<net>/`.
    Gen {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        out_dir: PathBuf,
        /// Path to `secrets.sops.yaml` (encrypted) or plaintext YAML. If encrypted, requires
        /// `sops` available on PATH.
        #[arg(long)]
        secrets_file: Option<PathBuf>,
        /// Allow generating bundles without providing validator archiver bearer tokens.
        ///
        /// By default, generation fails if any validator is missing a bearer token, because
        /// Ultramarine validators fail fast without it.
        #[arg(long, default_value_t = false)]
        allow_missing_archiver_tokens: bool,
        #[arg(long, default_value_t = false)]
        allow_unsafe_failure_domains: bool,
    },
}

#[derive(Clone, Debug, Deserialize)]
struct Manifest {
    schema_version: u32,
    network: Network,
    images: Images,
    hosts: Vec<Host>,
    nodes: Vec<Node>,
    engine: Engine,
    ports: Ports,
    sync: Sync,
    archiver: Archiver,
    exposure: Exposure,
}

#[derive(Clone, Debug, Deserialize)]
struct Network {
    name: String,
    chain_id: u64,
}

#[derive(Clone, Debug, Deserialize)]
struct Images {
    ultramarine: String,
    load_reth: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Host {
    id: String,
    public_ip: String,
    ssh_user: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Node {
    id: String,
    host: String,
    role: String, // validator|fullnode|rpc
}

#[derive(Clone, Debug, Deserialize)]
struct Engine {
    mode: String, // ipc
    ipc_path_template: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Ports {
    allocation: String, // host-block|by-index
    host_block_stride: Option<u16>,
    el: ElPorts,
    cl: ClPorts,
}

#[derive(Clone, Debug, Deserialize)]
struct ElPorts {
    http: u16,
    p2p: u16,
    metrics: u16,
}

#[derive(Clone, Debug, Deserialize)]
struct ClPorts {
    p2p: u16,
    mempool: u16,
    metrics: u16,
}

#[derive(Clone, Debug, Deserialize)]
struct Sync {
    enabled: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct Archiver {
    enabled: bool,
    provider_url: String,
    provider_id: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Exposure {
    metrics_bind: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Secrets {
    schema_version: u32,
    nodes: BTreeMap<String, NodeSecrets>,
}

#[derive(Clone, Debug, Deserialize)]
struct NodeSecrets {
    archiver_bearer_token: String,
}

#[derive(Clone, Debug, Serialize)]
struct Lockfile {
    schema_version: u32,
    tool: ToolInfo,
    network: LockNetwork,
    inputs: Inputs,
    policy: Policy,
    hosts: Vec<LockHost>,
    nodes: Vec<LockNode>,
    artifacts: Artifacts,
}

#[derive(Clone, Debug, Serialize)]
struct ToolInfo {
    name: &'static str,
    version: &'static str,
}

#[derive(Clone, Debug, Serialize)]
struct LockNetwork {
    name: String,
    chain_id: u64,
}

#[derive(Clone, Debug, Serialize)]
struct Inputs {
    manifest_path: String,
    manifest_sha256: String,
}

#[derive(Clone, Debug, Serialize)]
struct Policy {
    engine: &'static str,
    sync_enabled: bool,
    metrics_bind: String,
}

#[derive(Clone, Debug, Serialize)]
struct LockHost {
    id: String,
    public_ip: String,
    ssh_user: String,
}

#[derive(Clone, Debug, Serialize)]
struct LockNode {
    id: String,
    host: String,
    role: String,
    images: ImagesOut,
    engine: EngineOut,
    ports: PortsOut,
    load_reth: LoadRethOut,
    archiver: Option<ArchiverOut>,
}

#[derive(Clone, Debug, Serialize)]
struct ImagesOut {
    ultramarine: String,
    load_reth: String,
}

#[derive(Clone, Debug, Serialize)]
struct EngineOut {
    mode: &'static str,
    ipc_path: String,
}

#[derive(Clone, Debug, Serialize)]
struct PortsOut {
    el_http: u16,
    el_p2p: u16,
    el_metrics: u16,
    cl_p2p: u16,
    cl_mempool: u16,
    cl_metrics: u16,
}

#[derive(Clone, Debug, Serialize)]
struct LoadRethOut {
    p2p_key_path: String,
    enode: String,
    bootnodes: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct ArchiverOut {
    enabled: bool,
    provider_url: String,
    provider_id: String,
    bearer_token_present: bool,
}

#[derive(Clone, Debug, Serialize)]
struct Artifacts {
    public: PublicArtifacts,
}

#[derive(Clone, Debug, Serialize)]
struct NetworkJson {
    schema_version: u32,
    tool: ToolInfo,
    network: LockNetwork,
    nodes: Vec<NetworkNode>,
    artifacts: Artifacts,
}

#[derive(Clone, Debug, Serialize)]
struct NetworkNode {
    id: String,
    role: String,
    host: String,
    public_ip: String,
    ports: PortsOut,
    load_reth: NetworkLoadReth,
    archiver: Option<NetworkArchiver>,
}

#[derive(Clone, Debug, Serialize)]
struct NetworkLoadReth {
    enode: String,
    bootnodes: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct NetworkArchiver {
    enabled: bool,
    provider_url: String,
    provider_id: String,
}

#[derive(Clone, Debug, Serialize)]
struct PublicArtifacts {
    genesis_json: ArtifactRef,
}

#[derive(Clone, Debug, Serialize)]
struct ArtifactRef {
    path: String,
    sha256: String,
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn read_yaml<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let raw = fs::read_to_string(path)?;
    Ok(serde_yaml::from_str(&raw)?)
}

fn read_secrets(path: &Path) -> Result<Secrets> {
    let raw = if path.extension().and_then(|s| s.to_str()) == Some("yaml") &&
        path.file_name().and_then(|s| s.to_str()).unwrap_or("").ends_with(".sops.yaml")
    {
        let sops = which::which("sops").map_err(|_| {
            eyre!("secrets file looks like sops-encrypted, but `sops` is not available on PATH")
        })?;
        let out = Command::new(sops).args(["-d"]).arg(path).output()?;
        if !out.status.success() {
            bail!("sops -d failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        String::from_utf8(out.stdout)?
    } else {
        fs::read_to_string(path)?
    };

    Ok(serde_yaml::from_str(&raw)?)
}

fn validate_manifest(m: &Manifest, allow_unsafe_failure_domains: bool) -> Result<()> {
    if m.schema_version != 1 {
        bail!("schema_version must be 1");
    }
    if m.network.name.trim().is_empty() {
        bail!("network.name must be non-empty");
    }
    if m.images.ultramarine.trim().is_empty() || m.images.load_reth.trim().is_empty() {
        bail!("images.ultramarine/images.load_reth must be non-empty");
    }
    if m.engine.mode != "ipc" {
        bail!("engine.mode must be 'ipc' (deploys are IPC-only)");
    }
    if !m.engine.ipc_path_template.contains("{node_id}") {
        bail!("engine.ipc_path_template must contain '{{node_id}}' placeholder");
    }
    if !m.sync.enabled {
        bail!("sync.enabled must be true for multi-host networks");
    }
    if !m.archiver.enabled {
        bail!("archiver.enabled must be true (validators require archiver)");
    }
    if m.archiver.provider_url.trim().is_empty() || m.archiver.provider_id.trim().is_empty() {
        bail!("archiver.provider_url/provider_id must be non-empty");
    }

    match m.ports.allocation.as_str() {
        "host-block" | "by-index" => {}
        other => bail!("ports.allocation must be host-block|by-index (got {other})"),
    }
    if m.ports.allocation == "host-block" {
        let stride = m.ports.host_block_stride.unwrap_or(1_000);
        if stride == 0 {
            bail!("ports.host_block_stride must be >= 1");
        }
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for n in &m.nodes {
            *counts.entry(n.host.as_str()).or_default() += 1;
        }
        if let Some((host, count)) = counts.into_iter().max_by_key(|(_, c)| *c) {
            if count > stride as usize {
                bail!(
                    "ports.host_block_stride={} is too small: host {} has {} nodes; need >= {} to avoid port collisions",
                    stride,
                    host,
                    count,
                    count
                );
            }
        }
    }

    let mut host_ids = BTreeSet::new();
    for h in &m.hosts {
        if !host_ids.insert(h.id.clone()) {
            bail!("duplicate host id: {}", h.id);
        }
        if h.public_ip.trim().is_empty() || h.ssh_user.trim().is_empty() {
            bail!("host {} public_ip/ssh_user must be non-empty", h.id);
        }
    }
    if m.hosts.is_empty() {
        bail!("hosts must be non-empty");
    }

    let mut node_ids = BTreeSet::new();
    for n in &m.nodes {
        if !node_ids.insert(n.id.clone()) {
            bail!("duplicate node id: {}", n.id);
        }
        if !host_ids.contains(&n.host) {
            bail!("node {} references unknown host {}", n.id, n.host);
        }
        match n.role.as_str() {
            "validator" | "fullnode" | "rpc" => {}
            _ => bail!("node {} role must be validator|fullnode|rpc", n.id),
        }
    }
    if m.nodes.is_empty() {
        bail!("nodes must be non-empty");
    }

    // Failure-domain liveness math: strict >2/3 (Tendermint/Malachite).
    let validators: Vec<&Node> = m.nodes.iter().filter(|n| n.role == "validator").collect();
    if validators.is_empty() {
        bail!("at least one validator node is required");
    }
    let n_total = validators.len() as u64;
    let max_allowed = (n_total.saturating_sub(1) / 3) as usize;
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for v in validators {
        *counts.entry(v.host.as_str()).or_default() += 1;
    }
    let offenders: BTreeMap<&str, usize> =
        counts.into_iter().filter(|(_, c)| *c > max_allowed).collect();
    if !offenders.is_empty() {
        if n_total < 4 {
            eprintln!(
                "warning: validator placement cannot be resilient to losing any single host with n={} equal-weight validators (survival would require validators_per_host<=0). offenders={:?}",
                n_total, offenders
            );
        } else if !allow_unsafe_failure_domains {
            bail!(
                "unsafe validator placement across failure domains; validators_per_host should be <= {} for n={} to survive losing any single host; offenders={:?} (override with --allow-unsafe-failure-domains)",
                max_allowed,
                n_total,
                offenders
            );
        } else {
            eprintln!(
                "warning: unsafe validator placement across failure domains; offenders={:?}",
                offenders
            );
        }
    }

    Ok(())
}

fn engine_ipc_path(m: &Manifest, node_id: &str) -> String {
    m.engine.ipc_path_template.replace("{node_id}", node_id)
}

fn port_offset_host_block(m: &Manifest, node: &Node) -> Result<u16> {
    let stride = m.ports.host_block_stride.unwrap_or(1_000);
    let host_index = m
        .hosts
        .iter()
        .position(|h| h.id == node.host)
        .ok_or_else(|| eyre!("unknown host referenced: {}", node.host))?;
    let mut same_host: Vec<&Node> = m.nodes.iter().filter(|n| n.host == node.host).collect();
    same_host.sort_by(|a, b| a.id.cmp(&b.id));
    let local_index = same_host
        .iter()
        .position(|n| n.id == node.id)
        .ok_or_else(|| eyre!("node not found in host set"))?;
    let base = (host_index as u32) * (stride as u32);
    let off = base + (local_index as u32);
    Ok(u16::try_from(off).map_err(|_| eyre!("port offset overflow"))?)
}

fn port_offset_by_index(m: &Manifest, node: &Node) -> Result<u16> {
    let mut nodes: Vec<&Node> = m.nodes.iter().collect();
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    let idx = nodes.iter().position(|n| n.id == node.id).ok_or_else(|| eyre!("node not found"))?;
    Ok(u16::try_from(idx).map_err(|_| eyre!("node index overflow"))?)
}

fn ports_for_node(m: &Manifest, node: &Node) -> Result<PortsOut> {
    let off = match m.ports.allocation.as_str() {
        "host-block" => port_offset_host_block(m, node)?,
        "by-index" => port_offset_by_index(m, node)?,
        other => bail!("unsupported ports.allocation: {other}"),
    };

    fn add_port(base: u16, off: u16, what: &str) -> Result<u16> {
        let v = (base as u32) + (off as u32);
        if v > u16::MAX as u32 {
            bail!(
                "port allocation overflow for {what}: base={base} offset={off} => {v} (max {})",
                u16::MAX
            );
        }
        Ok(v as u16)
    }

    Ok(PortsOut {
        el_http: add_port(m.ports.el.http, off, "el.http")?,
        el_p2p: add_port(m.ports.el.p2p, off, "el.p2p")?,
        el_metrics: add_port(m.ports.el.metrics, off, "el.metrics")?,
        cl_p2p: add_port(m.ports.cl.p2p, off, "cl.p2p")?,
        cl_mempool: add_port(m.ports.cl.mempool, off, "cl.mempool")?,
        cl_metrics: add_port(m.ports.cl.metrics, off, "cl.metrics")?,
    })
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomic_with_mode(path, bytes, None)
}

fn write_atomic_with_mode(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    #[cfg(unix)]
    {
        use std::{
            io::Write,
            os::unix::fs::{OpenOptionsExt, PermissionsExt},
        };

        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        if let Some(m) = mode {
            opts.mode(m);
        }

        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;

        // Ensure mode even if the tmp file already existed.
        if let Some(m) = mode {
            fs::set_permissions(&tmp, fs::Permissions::from_mode(m))?;
        }
    }
    #[cfg(not(unix))]
    {
        fs::write(&tmp, bytes)?;
    }
    fs::rename(tmp, path)?;
    #[cfg(unix)]
    {
        if let Some(m) = mode {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(m))?;
        }
    }
    Ok(())
}

fn ensure_load_reth_p2p_key(private_dir: &Path, node_id: &str) -> Result<(PathBuf, String)> {
    let key_path = private_dir.join("load-reth").join("p2p-keys").join(format!("{node_id}.key"));

    let secret = if key_path.try_exists()? {
        let s = fs::read_to_string(&key_path)?;
        let hex_str = s.trim().trim_start_matches("0x");
        let bytes = hex::decode(hex_str)?;
        SecretKey::from_slice(&bytes).map_err(|_| eyre!("invalid p2p key bytes"))?
    } else {
        let secret = SecretKey::random(&mut rand::thread_rng());
        let bytes = secret.to_bytes();
        write_atomic_with_mode(&key_path, hex::encode(bytes).as_bytes(), Some(0o600))?;
        secret
    };

    // Tighten permissions if the key already existed with unsafe mode.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))?;
    }

    let public = secret.public_key();
    let encoded = public.to_encoded_point(false);
    let pub_bytes = encoded.as_bytes();
    if pub_bytes.first().copied() != Some(0x04) {
        bail!("expected uncompressed pubkey");
    }
    let enode_pub_hex = hex::encode(&pub_bytes[1..]);
    Ok((key_path, enode_pub_hex))
}

fn write_env_file(path: &Path, entries: &[(&str, String)], mode: Option<u32>) -> Result<()> {
    let mut out = String::new();
    for (k, v) in entries {
        let escaped = v.replace('\\', "\\\\").replace('"', "\\\"");
        out.push_str(k);
        out.push_str("=\"");
        out.push_str(&escaped);
        out.push_str("\"\n");
    }
    write_atomic_with_mode(path, out.as_bytes(), mode)
}

fn render_inventory(lock: &Lockfile) -> Result<String> {
    // Render as a standard static YAML inventory (no dynamic `_meta`), so Ansible
    // consistently applies per-host vars (ansible_host/ansible_user/loadnet_nodes).
    let mut hosts = serde_yaml::Mapping::new();

    let mut by_host: BTreeMap<&str, Vec<&LockNode>> = BTreeMap::new();
    for n in &lock.nodes {
        by_host.entry(n.host.as_str()).or_default().push(n);
    }

    for h in &lock.hosts {
        let mut hv = serde_yaml::Mapping::new();
        hv.insert(
            serde_yaml::Value::String("ansible_host".into()),
            serde_yaml::Value::String(h.public_ip.clone()),
        );
        hv.insert(
            serde_yaml::Value::String("ansible_user".into()),
            serde_yaml::Value::String(h.ssh_user.clone()),
        );

        let mut nodes: Vec<String> =
            by_host.get(h.id.as_str()).into_iter().flatten().map(|n| n.id.clone()).collect();
        nodes.sort();
        hv.insert(
            serde_yaml::Value::String("loadnet_nodes".into()),
            serde_yaml::Value::Sequence(nodes.into_iter().map(serde_yaml::Value::String).collect()),
        );

        hosts.insert(serde_yaml::Value::String(h.id.clone()), serde_yaml::Value::Mapping(hv));
    }

    let mut all = serde_yaml::Mapping::new();
    all.insert(serde_yaml::Value::String("hosts".into()), serde_yaml::Value::Mapping(hosts));
    all.insert(
        serde_yaml::Value::String("vars".into()),
        serde_yaml::Value::Mapping(Default::default()),
    );
    all.insert(
        serde_yaml::Value::String("children".into()),
        serde_yaml::Value::Mapping(Default::default()),
    );

    let mut root = serde_yaml::Mapping::new();
    root.insert(serde_yaml::Value::String("all".into()), serde_yaml::Value::Mapping(all));

    Ok(serde_yaml::to_string(&serde_yaml::Value::Mapping(root))?)
}

fn generate(
    manifest_path: &Path,
    out_dir: &Path,
    secrets_path: Option<&Path>,
    allow_missing_archiver_tokens: bool,
    allow_unsafe: bool,
) -> Result<()> {
    let manifest: Manifest = read_yaml(manifest_path)?;
    validate_manifest(&manifest, allow_unsafe)?;

    let secrets = if let Some(p) = secrets_path {
        let s = read_secrets(p)?;
        if s.schema_version != 1 {
            bail!("secrets schema_version must be 1");
        }
        Some(s)
    } else {
        None
    };

    // Critical invariant: validators require archiver bearer tokens (Ultramarine fails fast).
    if !allow_missing_archiver_tokens {
        let mut missing: Vec<String> = Vec::new();
        for n in &manifest.nodes {
            if n.role != "validator" {
                continue;
            }
            let tok = secrets
                .as_ref()
                .and_then(|s| s.nodes.get(&n.id))
                .map(|ns| ns.archiver_bearer_token.trim())
                .unwrap_or("");
            if tok.is_empty() {
                missing.push(n.id.clone());
            }
        }
        if !missing.is_empty() {
            bail!(
                "missing archiver bearer tokens for validator nodes: {:?}. Provide --secrets-file or pass --allow-missing-archiver-tokens (unsafe).",
                missing
            );
        }
    }

    let manifest_sha = sha256_file(manifest_path)?;

    let public_dir = out_dir.join("bundle").join("public");
    let private_dir = out_dir.join("bundle").join("private");
    let env_dir = private_dir.join("env");
    let ultra_homes_dir = private_dir.join("ultramarine").join("homes");

    // Public artifact: EL genesis.
    let genesis = ultramarine_genesis::build_dev_genesis(manifest.network.chain_id)?;
    let genesis_path = public_dir.join("genesis.json");
    ultramarine_genesis::write_genesis(&genesis_path, &genesis)?;
    let genesis_sha = sha256_file(&genesis_path)?;

    // Generate (or reuse) per-node signing keys and build the CL genesis validator set.
    //
    // Note: all nodes (including non-validators) need a `priv_validator_key.json` today because
    // Ultramarine derives its libp2p identity from this key.
    let mut nodes_by_id: Vec<&Node> = manifest.nodes.iter().collect();
    nodes_by_id.sort_by(|a, b| a.id.cmp(&b.id));

    let mut genesis_validators: Vec<Validator> = Vec::new();
    for n in &nodes_by_id {
        let key_path = ultra_homes_dir.join(&n.id).join("config").join("priv_validator_key.json");
        let pk: ConsensusPrivateKey = if key_path.try_exists()? {
            serde_json::from_str(&fs::read_to_string(&key_path)?)?
        } else {
            let pk = ConsensusPrivateKey::generate(rand::rngs::OsRng);
            write_atomic_with_mode(
                &key_path,
                serde_json::to_string_pretty(&pk)?.as_bytes(),
                Some(0o600),
            )?;
            pk
        };
        if n.role == "validator" {
            genesis_validators.push(Validator::new(pk.public_key(), 1u64.into()));
        }
    }
    let consensus_genesis =
        ConsensusGenesis { validator_set: ValidatorSet::new(genesis_validators) };

    // Emit Ultramarine node homes (config/config.toml + config/genesis.json).
    for n in &manifest.nodes {
        let ports = ports_for_node(&manifest, n)?;
        let node_home = ultra_homes_dir.join(&n.id);
        let config_dir = node_home.join("config");

        // genesis.json
        write_atomic(
            &config_dir.join("genesis.json"),
            serde_json::to_string_pretty(&consensus_genesis)?.as_bytes(),
        )?;

        // config.toml (Ultramarine CLI config wrapper)
        let mut cfg = ultramarine_cli::config::Config::default();
        cfg.moniker = n.id.clone();
        cfg.metrics.enabled = true;
        cfg.metrics.listen_addr =
            format!("{}:{}", manifest.exposure.metrics_bind, ports.cl_metrics)
                .parse()
                .map_err(|e| eyre!("invalid metrics listen addr: {e}"))?;

        let transport = ultramarine_cli::config::TransportProtocol::Tcp;
        cfg.consensus.p2p.listen_addr = transport.multiaddr("0.0.0.0", ports.cl_p2p as usize);
        cfg.mempool.p2p.listen_addr = transport.multiaddr("0.0.0.0", ports.cl_mempool as usize);

        // Deterministic persistent peers (dialable public IPs).
        let mut consensus_peers = Vec::new();
        let mut mempool_peers = Vec::new();
        for other in &manifest.nodes {
            if other.id == n.id {
                continue;
            }
            let other_ports = ports_for_node(&manifest, other)?;
            let other_ip = host_ip(&manifest, &other.host)?;
            consensus_peers.push(transport.multiaddr(&other_ip, other_ports.cl_p2p as usize));
            mempool_peers.push(transport.multiaddr(&other_ip, other_ports.cl_mempool as usize));
        }
        consensus_peers.sort();
        mempool_peers.sort();
        cfg.consensus.p2p.persistent_peers = consensus_peers;
        cfg.mempool.p2p.persistent_peers = mempool_peers;

        // Multi-host requires ValueSync.
        cfg.sync.enabled = true;

        // Archiver baseline config (token is supplied via env for validators).
        cfg.archiver.enabled = n.role == "validator";
        cfg.archiver.provider_url = manifest.archiver.provider_url.clone();
        cfg.archiver.provider_id = manifest.archiver.provider_id.clone();

        write_atomic(&config_dir.join("config.toml"), toml::to_string_pretty(&cfg)?.as_bytes())?;
    }

    let mut hosts: Vec<LockHost> = manifest
        .hosts
        .iter()
        .map(|h| LockHost {
            id: h.id.clone(),
            public_ip: h.public_ip.clone(),
            ssh_user: h.ssh_user.clone(),
        })
        .collect();
    hosts.sort_by(|a, b| a.id.cmp(&b.id));

    let mut nodes_sorted: Vec<&Node> = manifest.nodes.iter().collect();
    nodes_sorted.sort_by(|a, b| a.id.cmp(&b.id));

    let mut lock_nodes: Vec<LockNode> = Vec::new();
    for n in nodes_sorted {
        let ports = ports_for_node(&manifest, n)?;
        let ipc_path = engine_ipc_path(&manifest, &n.id);

        let (key_path, enode_pub_hex) = ensure_load_reth_p2p_key(&private_dir, &n.id)?;
        let enode =
            format!("enode://{enode_pub_hex}@{}:{}", host_ip(&manifest, &n.host)?, ports.el_p2p);

        let p2p_key_rel =
            key_path.strip_prefix(out_dir).unwrap_or(&key_path).to_string_lossy().to_string();

        lock_nodes.push(LockNode {
            id: n.id.clone(),
            host: n.host.clone(),
            role: n.role.clone(),
            images: ImagesOut {
                ultramarine: manifest.images.ultramarine.clone(),
                load_reth: manifest.images.load_reth.clone(),
            },
            engine: EngineOut { mode: "ipc", ipc_path },
            ports,
            load_reth: LoadRethOut { p2p_key_path: p2p_key_rel, enode, bootnodes: vec![] },
            archiver: if n.role == "validator" {
                Some(ArchiverOut {
                    enabled: true,
                    provider_url: manifest.archiver.provider_url.clone(),
                    provider_id: manifest.archiver.provider_id.clone(),
                    bearer_token_present: secrets.is_some(),
                })
            } else {
                None
            },
        });
    }

    // Compute bootnodes (all enodes except self).
    let all_enodes: Vec<String> = lock_nodes.iter().map(|n| n.load_reth.enode.clone()).collect();
    for n in &mut lock_nodes {
        n.load_reth.bootnodes =
            all_enodes.iter().filter(|e| *e != &n.load_reth.enode).cloned().collect();
    }

    let lock = Lockfile {
        schema_version: 1,
        tool: ToolInfo { name: "netgen", version: env!("CARGO_PKG_VERSION") },
        network: LockNetwork {
            name: manifest.network.name.clone(),
            chain_id: manifest.network.chain_id,
        },
        inputs: Inputs {
            manifest_path: manifest_path.display().to_string(),
            manifest_sha256: manifest_sha,
        },
        policy: Policy {
            engine: "ipc-only",
            sync_enabled: true,
            metrics_bind: manifest.exposure.metrics_bind.clone(),
        },
        hosts,
        nodes: lock_nodes,
        artifacts: Artifacts {
            public: PublicArtifacts {
                genesis_json: ArtifactRef {
                    path: genesis_path
                        .strip_prefix(out_dir)
                        .unwrap_or(&genesis_path)
                        .display()
                        .to_string(),
                    sha256: genesis_sha,
                },
            },
        },
    };

    // Write lockfile and public network JSON.
    let lock_path = out_dir.join("network.lock.json");
    write_atomic(&lock_path, serde_json::to_string_pretty(&lock)?.as_bytes())?;
    let network_json_path = public_dir.join("network.json");
    let mut host_ip_by_id: BTreeMap<&str, &str> = BTreeMap::new();
    for h in &lock.hosts {
        host_ip_by_id.insert(h.id.as_str(), h.public_ip.as_str());
    }
    let mut public_nodes: Vec<NetworkNode> = Vec::new();
    for n in &lock.nodes {
        let public_ip = host_ip_by_id
            .get(n.host.as_str())
            .ok_or_else(|| eyre!("lockfile missing host entry: {}", n.host))?
            .to_string();
        public_nodes.push(NetworkNode {
            id: n.id.clone(),
            role: n.role.clone(),
            host: n.host.clone(),
            public_ip,
            ports: n.ports.clone(),
            load_reth: NetworkLoadReth {
                enode: n.load_reth.enode.clone(),
                bootnodes: n.load_reth.bootnodes.clone(),
            },
            archiver: n.archiver.as_ref().map(|a| NetworkArchiver {
                enabled: a.enabled,
                provider_url: a.provider_url.clone(),
                provider_id: a.provider_id.clone(),
            }),
        });
    }
    let public_network = NetworkJson {
        schema_version: 1,
        tool: lock.tool.clone(),
        network: lock.network.clone(),
        nodes: public_nodes,
        artifacts: lock.artifacts.clone(),
    };
    write_atomic(&network_json_path, serde_json::to_string_pretty(&public_network)?.as_bytes())?;

    // Inventory.
    let inv = render_inventory(&lock)?;
    write_atomic(&out_dir.join("inventory.yml"), inv.as_bytes())?;

    // Per-node runtime env files (consumed by systemd/Ansible).
    for node in &lock.nodes {
        let ultramarine_env_path = env_dir.join(format!("ultramarine-{}.env", node.id));
        let load_reth_env_path = env_dir.join(format!("load-reth-{}.env", node.id));

        let mut ultra_entries: Vec<(&str, String)> = Vec::new();
        ultra_entries.push(("ULTRAMARINE_NODE_ID", node.id.clone()));
        ultra_entries.push((
            "ULTRAMARINE_HOME_DIR",
            format!("bundle/private/ultramarine/homes/{}", node.id),
        ));
        ultra_entries.push(("ULTRAMARINE_ENGINE_IPC_PATH", node.engine.ipc_path.clone()));
        ultra_entries
            .push(("ULTRAMARINE_ETH1_RPC_URL", format!("http://127.0.0.1:{}", node.ports.el_http)));
        ultra_entries.push(("ULTRAMARINE_METRICS_BIND", manifest.exposure.metrics_bind.clone()));
        ultra_entries.push(("ULTRAMARINE_CL_P2P_PORT", node.ports.cl_p2p.to_string()));
        ultra_entries.push(("ULTRAMARINE_CL_MEMPOOL_PORT", node.ports.cl_mempool.to_string()));
        ultra_entries.push(("ULTRAMARINE_CL_METRICS_PORT", node.ports.cl_metrics.to_string()));
        ultra_entries.push(("ULTRAMARINE_IMAGE", node.images.ultramarine.clone()));

        if node.role == "validator" {
            ultra_entries.push(("ULTRAMARINE_ARCHIVER_ENABLED", "true".to_string()));
            ultra_entries.push((
                "ULTRAMARINE_ARCHIVER_PROVIDER_URL",
                manifest.archiver.provider_url.clone(),
            ));
            ultra_entries
                .push(("ULTRAMARINE_ARCHIVER_PROVIDER_ID", manifest.archiver.provider_id.clone()));

            if let Some(s) = &secrets {
                let tok = s
                    .nodes
                    .get(&node.id)
                    .ok_or_else(|| eyre!("missing secrets.nodes.{}", node.id))?
                    .archiver_bearer_token
                    .trim()
                    .to_string();
                if tok.is_empty() {
                    bail!("empty archiver token for node {}", node.id);
                }
                let secret_path = private_dir
                    .join("ultramarine")
                    .join("secrets")
                    .join(format!("{}.env", node.id));
                write_env_file(
                    &secret_path,
                    &[("ULTRAMARINE_ARCHIVER_BEARER_TOKEN", tok)],
                    Some(0o600),
                )?;
            }
        }
        write_env_file(&ultramarine_env_path, &ultra_entries, None)?;

        let mut reth_entries: Vec<(&str, String)> = Vec::new();
        reth_entries.push(("LOAD_RETH_NODE_ID", node.id.clone()));
        reth_entries.push(("LOAD_RETH_IMAGE", node.images.load_reth.clone()));
        reth_entries.push(("LOAD_RETH_PUBLIC_IP", host_ip(&manifest, &node.host)?));
        reth_entries.push(("LOAD_RETH_HTTP_PORT", node.ports.el_http.to_string()));
        reth_entries.push(("LOAD_RETH_P2P_PORT", node.ports.el_p2p.to_string()));
        reth_entries.push(("LOAD_RETH_METRICS_PORT", node.ports.el_metrics.to_string()));
        reth_entries.push(("LOAD_RETH_ENGINE_IPC_PATH", node.engine.ipc_path.clone()));
        reth_entries.push(("LOAD_RETH_P2P_KEY_PATH", node.load_reth.p2p_key_path.clone()));
        reth_entries.push(("LOAD_RETH_BOOTNODES", node.load_reth.bootnodes.join(",")));
        reth_entries
            .push(("LOAD_RETH_GENESIS_JSON", lock.artifacts.public.genesis_json.path.clone()));
        write_env_file(&load_reth_env_path, &reth_entries, None)?;
    }

    println!("wrote {}", lock_path.display());
    Ok(())
}

fn host_ip(m: &Manifest, host_id: &str) -> Result<String> {
    m.hosts
        .iter()
        .find(|h| h.id == host_id)
        .map(|h| h.public_ip.clone())
        .ok_or_else(|| eyre!("unknown host id: {host_id}"))
}

fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Validate { manifest, allow_unsafe_failure_domains } => {
            let m: Manifest = read_yaml(&manifest)?;
            validate_manifest(&m, allow_unsafe_failure_domains)?;
            println!("ok");
            Ok(())
        }
        Cmd::Gen {
            manifest,
            out_dir,
            secrets_file,
            allow_missing_archiver_tokens,
            allow_unsafe_failure_domains,
        } => generate(
            &manifest,
            &out_dir,
            secrets_file.as_deref(),
            allow_missing_archiver_tokens,
            allow_unsafe_failure_domains,
        ),
    }
}
