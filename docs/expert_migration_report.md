# Ultramarine → Malachite Upgrade Assessment

## 1. Documentation Findings
- `docs/MALACHITE_UPGRADE_STATUS.md` still flags the `malachitebft_app::Node` import move, but every CLI module already targets `malachitebft_app::node::Node` (e.g. `crates/cli/src/file.rs:5`, `crates/cli/src/cmd/distributed_testnet.rs:8`). The checklist should be updated to reflect completion.
- `ultramarine/crates/node/src/node.rs` continues to depend on `malachitebft_app::types::config::Config` and the pre-0.3 `Node` trait surface. The latest Malachite expects an application-specific `Node::Config` that implements `NodeConfig`, plus async/`eyre::Result` loaders and dual codec parameters for `start_engine` (`malachite/code/crates/app/src/node.rs`).
- The repo’s signing glue (`crates/types/src/signing.rs`) still implements the deprecated synchronous `SigningProvider` trait from `malachitebft_core_types`. Upstream moved signing to `malachitebft_signing` with async methods returning `Result<VerificationResult, Error>`.
- Sync protocol types and codecs lag behind Malachite 0.3+; `ValueRequest` emits `start_height/end_height`, commit certificates expect `AggregatedSignature`, and codec helpers for VoteSet remain (`crates/types/src/codec/proto/mod.rs`). Malachite dropped VoteSet support and now ships raw commit signature lists.

## 2. Code Flow Trace
- CLI configuration is generated in `crates/cli/src/new.rs:55` and persisted via `file::save_config`. Commands (`init`, `testnet`, `distributed_testnet`) feed those configs into `bin/ultramarine/src/main.rs`, which parses TOML and sets up runtime/logging before instantiating `ultramarine_node::node::App`.
- The node bridges into Malachite through `malachitebft_app_channel::start_engine` (`crates/node/src/node.rs:158`). That handoff provides consensus/sync codecs (`crates/types/src/codec/proto/mod.rs`) and spawns the execution client/metrics integration.
- Execution-layer wiring (`ExecutionClient`, JWT handling) and blob engine setup (`BlobEngineImpl`) live inside `App::start`, illustrating the tight coupling between Ultramarine application state and Malachite’s consensus engine.

## 3. Migration Work Plan

### A. Configuration Refactor
1. Introduce `UltramarineConfig` mirroring the upstream channel example and implement `NodeConfig`:
   ```rust
   #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
   pub struct UltramarineConfig {
       pub moniker: String,
       pub logging: LoggingConfig,
       pub consensus: ConsensusConfig,
       pub value_sync: ValueSyncConfig,
       pub mempool: MempoolConfig,
       pub metrics: MetricsConfig,
       pub runtime: RuntimeConfig,
       pub test: TestConfig,
   }

   pub fn load_config(path: impl AsRef<Path>, prefix: Option<&str>) -> eyre::Result<UltramarineConfig> {
       ::config::Config::builder()
           .add_source(::config::File::from(path.as_ref()))
           .add_source(::config::Environment::with_prefix(prefix.unwrap_or("ULTRAMARINE")).separator("__"))
           .build()?;
       // … try_deserialize()
   }
   ```
2. Update CLI generators (`new.rs`, `cmd/init.rs`, `cmd/testnet.rs`, `cmd/distributed_testnet.rs`) and serializers (`file.rs`) to create and persist the new struct, mapping legacy `sync` usage onto `ValueSyncConfig` (e.g. disable value sync where previous `SyncConfig` set `enabled: false`).

### B. Node Trait Compliance
1. Set `type Config = UltramarineConfig` inside `App` and add `fn load_config(&self) -> eyre::Result<Self::Config>` reading from disk.
2. Adjust `load_private_key_file`/`load_genesis` to return `eyre::Result` per new trait signature.
3. Call the updated `start_engine` with both WAL and network codecs:
   ```rust
   let (mut channels, engine_handle) = malachitebft_app_channel::start_engine(
       ctx.clone(),
       self.clone(),
       config.clone(),
       ProtobufCodec, // WAL codec
       ProtobufCodec, // network codec
       self.start_height,
       initial_validator_set,
   ).await?;
   ```

### C. Signing Provider Upgrade
1. Add `to_sign_bytes`/`from_sign_bytes` helpers to `Vote` and `Proposal` (currently only `to_bytes`).
2. Re-implement `Ed25519Provider` against `malachitebft_signing::SigningProvider`:
   ```rust
   #[async_trait]
   impl SigningProvider<LoadContext> for Ed25519Provider {
       async fn sign_vote(&self, vote: Vote) -> Result<SignedVote<LoadContext>, Error> {
           Ok(SignedVote::new(vote.clone(), self.sign(&vote.to_sign_bytes())))
       }

       async fn verify_signed_vote(
           &self,
           vote: &Vote,
           signature: &Signature,
           public_key: &PublicKey,
       ) -> Result<VerificationResult, Error> {
           Ok(VerificationResult::from_bool(public_key.verify(&vote.to_sign_bytes(), signature).is_ok()))
       }

       // repeat for proposals, parts, extensions…
   }
   ```
3. Drop the legacy `verify_commit_signature` helper—Malachite now verifies commit certificates internally using the raw signature list.

### D. Sync Protocol Alignment
1. Update `crates/types/proto/sync.proto`:
   ```proto
   message ValueRequest {
       uint64 height = 1;
       optional uint64 end_height = 2;
   }

   message CommitCertificate {
       uint64 height = 1;
       uint32 round = 2;
       ValueId value_id = 3;
       repeated CommitSignature signatures = 4;
   }
   ```
   Remove VoteSet request/response messages entirely.
2. Regenerate Protobuf bindings and clean up codec helpers (`encode_vote_set`, `AggregatedSignature` logic) to work with `commit_signatures: Vec<CommitSignature>` and `Option<Height>` handling that matches upstream (`malachite/code/crates/test/src/codec/proto/mod.rs`).
3. Update `ultramarine/crates/types/src/codec/proto/mod.rs::decode_vote` to return `Result` rather than `Option`, mirroring Malachite’s error propagation.

### E. Verification & Follow-up
1. Run `cargo fmt` and targeted `cargo check -p ultramarine-node -p ultramarine-cli`.
2. Regenerate Protobufs (`cargo build -p ultramarine-types --features proto-build`).
3. Update `docs/MALACHITE_UPGRADE_STATUS.md` with the resolved Node import item and the new task breakdown above.

## 4. Suggested Next Steps
1. Implement the configuration/signing/sync refactors, regenerate protobufs, and run `cargo check` for `ultramarine-node` and `ultramarine-cli`.
2. After compilation succeeds, refresh `docs/MALACHITE_UPGRADE_STATUS.md` to mark completed work and note the new blockers.
