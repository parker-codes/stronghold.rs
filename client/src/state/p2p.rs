// Copyright 2020-2021 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

use crate::{
    actors::{
        secure_messages::{
            CheckRecord, CheckVault, DeleteFromStore, ListIds, Procedures, ReadFromStore, RevokeData, WriteToStore,
            WriteToVault,
        },
        RecordError, Registry,
    },
    enum_from_inner,
    procedures::{self, ProcedureError, ProcedureOutput, StrongholdProcedure},
    Location, RecordHint, RecordId,
};
use actix::prelude::*;
use futures::channel::mpsc;
use p2p::{
    firewall::{FirewallConfiguration, FirewallRules, FwRequest, Rule},
    AddressInfo, ChannelSinkConfig, ConnectionLimits, EventChannel, InitKeypair, PeerId, ReceiveRequest, StrongholdP2p,
    StrongholdP2pBuilder,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{collections::HashMap, convert::TryFrom, io, marker::PhantomData, sync::Arc, time::Duration};

#[cfg(test)]
use crate::actors::secure_testing::ReadFromVault;

macro_rules! sh_result_mapping {
    ($enum:ident::$variant:ident => $inner:ty) => {
        impl From<$inner> for $enum {
            fn from(i: $inner) -> Self {
                $enum::$variant(i)
            }
        }
        impl TryFrom<$enum> for $inner {
            type Error = ();
            fn try_from(t: $enum) -> Result<Self, Self::Error> {
                if let $enum::$variant(v) = t {
                    Ok(v)
                } else {
                    Err(())
                }
            }
        }
    };
}

/// Actor that handles all network interaction.
///
/// On [`Network::new`] a new [`StrongholdP2p`] is created, which will spawn
/// a libp2p Swarm and continuously poll it.
pub struct Network {
    // Interface of stronghold-p2p for all network interaction.
    pub network: StrongholdP2p<ShRequest, ShResult, AccessRequest>,
    // Actor registry from which the address of the target client and snapshot actor can be queried.
    pub registry: Addr<Registry>,
    // Channel through which inbound requests are received.
    // This channel is only inserted temporary on [`Network::new`], and is handed
    // to the stream handler in `<Self as Actor>::started`.
    pub _inbound_request_rx: Option<mpsc::Receiver<ReceiveRequest<ShRequest, ShResult>>>,
    // Cache the network config so it can be returned on `ExportConfig`.
    pub _config: NetworkConfig,
}

impl Network {
    pub async fn new(
        registry: Addr<Registry>,
        mut network_config: NetworkConfig,
        keypair: Option<InitKeypair>,
    ) -> Result<Self, io::Error> {
        let (firewall_tx, _) = mpsc::channel(0);
        let (inbound_request_tx, inbound_request_rx) = EventChannel::new(10, ChannelSinkConfig::BufferLatest);
        let firewall_default = FirewallRules {
            inbound: Some(network_config.permissions_default.clone().into_rule()),
            outbound: Some(Rule::AllowAll),
        };
        let peer_permissions = network_config
            .peer_permissions
            .clone()
            .into_iter()
            .map(|(peer, permissions)| {
                let rules = FirewallRules {
                    inbound: Some(permissions.into_rule()),
                    outbound: Some(Rule::AllowAll),
                };
                (peer, rules)
            })
            .collect();
        let firewall_config = FirewallConfiguration {
            default: firewall_default,
            peer_rules: peer_permissions,
        };
        let mut builder = StrongholdP2pBuilder::new(firewall_tx, inbound_request_tx, None, firewall_config)
            .with_mdns_support(network_config.enable_mdns)
            .with_relay_support(network_config.enable_relay);
        if let Some(address_info) = network_config.addresses.take() {
            builder = builder.load_addresses(address_info);
        };
        if let Some(timeout) = network_config.request_timeout {
            builder = builder.with_request_timeout(timeout)
        }
        if let Some(timeout) = network_config.connection_timeout {
            builder = builder.with_connection_timeout(timeout)
        }
        if let Some(ref limit) = network_config.connections_limit {
            builder = builder.with_connections_limit(limit.clone())
        }
        if let Some(keypair) = keypair {
            builder = builder.with_keys(keypair);
        }

        let network = builder.build().await?;
        let actor = Self {
            network,
            _inbound_request_rx: Some(inbound_request_rx),
            registry,
            _config: network_config,
        };
        Ok(actor)
    }
}

/// Config for the new network.
///
/// Note: [`Default`] is implemented for [`NetworkConfig`] as [`NetworkConfig::new`] with [`Permissions::allow_none()`].
#[derive(Debug, Default, Clone)]
pub struct NetworkConfig {
    request_timeout: Option<Duration>,
    connection_timeout: Option<Duration>,
    connections_limit: Option<ConnectionLimits>,
    enable_mdns: bool,
    enable_relay: bool,
    addresses: Option<AddressInfo>,

    pub(crate) peer_permissions: HashMap<PeerId, Permissions>,
    pub(crate) permissions_default: Permissions,
}

impl Serialize for NetworkConfig {
    fn serialize<S>(&self, _: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        todo!()
    }
}

impl<'de> Deserialize<'de> for NetworkConfig {
    fn deserialize<D>(_: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        todo!()
    }
}

impl NetworkConfig {
    /// Create new network config with the given permission and default config:
    /// - No limit for simultaneous connections.
    /// - Request-timeout and Connection-timeout are 10s.
    /// - [`Mdns`][`libp2p::mdns`] protocol is disabled. **Note**: Enabling mdns will broadcast our own address and id
    ///   to the local network.
    /// - [`Relay`][`libp2p::relay`] functionality is disabled.
    pub fn new(default_permissions: Permissions) -> Self {
        NetworkConfig {
            permissions_default: default_permissions,
            ..Default::default()
        }
    }

    /// Set a timeout for receiving a response after a request was sent.
    ///
    /// This applies for inbound and outbound requests.
    pub fn with_request_timeout(mut self, t: Duration) -> Self {
        self.request_timeout = Some(t);
        self
    }

    /// Set the limit for simultaneous connections.
    /// By default no connection limits apply.
    pub fn with_connections_limit(mut self, limit: ConnectionLimits) -> Self {
        self.connections_limit = Some(limit);
        self
    }

    /// Set the timeout for a idle connection to a remote peer.
    pub fn with_connection_timeout(mut self, t: Duration) -> Self {
        self.connection_timeout = Some(t);
        self
    }

    /// Enable / Disable [`Mdns`][`libp2p::mdns`] protocol.
    /// **Note**: Enabling mdns will broadcast our own address and id to the local network.
    pub fn with_mdns_enabled(mut self, is_enabled: bool) -> Self {
        self.enable_mdns = is_enabled;
        self
    }

    /// Enable / Disable [`Relay`][`libp2p::relay`] functionality.
    /// This also means that other peers can use us as relay/
    pub fn with_relay_enabled(mut self, is_enabled: bool) -> Self {
        self.enable_relay = is_enabled;
        self
    }

    /// Import known addresses and relays from a past network actor.
    pub fn with_address_info(mut self, info: AddressInfo) -> Self {
        self.addresses = Some(info);
        self
    }
}

#[derive(Debug, Message, Clone, Serialize, Deserialize)]
#[rtype(result = "Result<(), RemoteRecordError>")]
pub struct WriteToRemoteVault {
    pub location: Location,
    pub payload: Vec<u8>,
    pub hint: RecordHint,
}

impl From<WriteToRemoteVault> for WriteToVault {
    fn from(t: WriteToRemoteVault) -> Self {
        let WriteToRemoteVault {
            location,
            payload,
            hint,
        } = t;
        WriteToVault {
            location,
            payload,
            hint,
        }
    }
}

impl From<WriteToVault> for WriteToRemoteVault {
    fn from(t: WriteToVault) -> Self {
        let WriteToVault {
            location,
            payload,
            hint,
        } = t;
        WriteToRemoteVault {
            location,
            payload,
            hint,
        }
    }
}

pub type RemoteRecordError = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShRequest {
    pub client_path: Vec<u8>,
    pub request: Request,
}

// Wrapper for Requests to a remote Secure Client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    CheckVault(CheckVault),
    CheckRecord(CheckRecord),
    ListIds(ListIds),
    #[cfg(test)]
    ReadFromVault(ReadFromVault),
    WriteToRemoteVault(WriteToRemoteVault),
    RevokeData(RevokeData),
    ReadFromStore(ReadFromStore),
    WriteToStore(WriteToStore),
    DeleteFromStore(DeleteFromStore),
    Procedures(Procedures),
}

enum_from_inner!(Request from CheckVault);
enum_from_inner!(Request from ListIds);
#[cfg(test)]
enum_from_inner!(Request from ReadFromVault);
enum_from_inner!(Request from WriteToRemoteVault);
enum_from_inner!(Request from RevokeData);
enum_from_inner!(Request from ReadFromStore);
enum_from_inner!(Request from WriteToStore);
enum_from_inner!(Request from DeleteFromStore);
enum_from_inner!(Request from Procedures);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ShResult {
    Empty(()),
    Data(Option<Vec<u8>>),
    Bool(bool),
    WriteRemoteVault(Result<(), RemoteRecordError>),
    ListIds(Vec<(RecordId, RecordHint)>),
    Proc(Result<Vec<ProcedureOutput>, ProcedureError>),
}

sh_result_mapping!(ShResult::Empty => ());
sh_result_mapping!(ShResult::Bool => bool);
sh_result_mapping!(ShResult::Data => Option<Vec<u8>>);
sh_result_mapping!(ShResult::ListIds => Vec<(RecordId, RecordHint)>);
sh_result_mapping!(ShResult::Proc => Result<Vec<ProcedureOutput>, ProcedureError>);

impl From<Result<(), RecordError>> for ShResult {
    fn from(inner: Result<(), RecordError>) -> Self {
        ShResult::WriteRemoteVault(inner.map_err(|e| e.to_string()))
    }
}

impl TryFrom<ShResult> for Result<(), RemoteRecordError> {
    type Error = ();
    fn try_from(t: ShResult) -> Result<Self, Self::Error> {
        if let ShResult::WriteRemoteVault(result) = t {
            Ok(result)
        } else {
            Err(())
        }
    }
}

/// Permissions for remote peers to operate on the local vault or store of a client.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Permissions {
    default: Option<ClientPermissions>,
    exceptions: HashMap<Vec<u8>, Option<ClientPermissions>>,
}

impl Permissions {
    /// No operations are permitted.
    pub fn allow_none() -> Self {
        Self::default()
    }

    /// All operations on all clients are permitted, including reading, writing and cloning secrets and reading/ writing
    /// to the store,
    pub fn allow_all() -> Self {
        Self {
            default: Some(ClientPermissions::all()),
            ..Default::default()
        }
    }

    /// Set default permissions for accessing all clients without any explicit rules.
    pub fn with_default_permissions(mut self, permissions: Option<ClientPermissions>) -> Self {
        self.default = permissions;
        self
    }

    /// Set specific permissions for access to the client at `client_path`.
    pub fn with_client_permissions(mut self, client_path: Vec<u8>, permissions: Option<ClientPermissions>) -> Self {
        self.exceptions.insert(client_path, permissions);
        self
    }

    pub(crate) fn into_rule(self) -> Rule<AccessRequest> {
        let restriction = move |rq: &AccessRequest| rq.check(self.clone());
        Rule::Restricted {
            restriction: Arc::new(restriction),
            _maker: PhantomData,
        }
    }
}

/// Restrict access to the vaults and store of a specific client.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ClientPermissions {
    use_vault_default: bool,
    use_vault_exceptions: HashMap<Vec<u8>, bool>,

    write_vault_default: bool,
    write_vault_exceptions: HashMap<Vec<u8>, bool>,

    clone_vault_default: bool,
    clone_vault_exceptions: HashMap<Vec<u8>, bool>,

    read_store: bool,
    write_store: bool,
}

impl ClientPermissions {
    /// No access to any structures of the vault is permitted.
    pub fn none() -> Self {
        Self::default()
    }
    /// All operations on the client are permitted, including reading, writing and cloning secrets and reading/ writing
    /// to the store,
    pub fn all() -> Self {
        ClientPermissions {
            use_vault_default: true,
            write_vault_default: true,
            clone_vault_default: true,
            read_store: true,
            write_store: true,
            ..Default::default()
        }
    }

    /// Set default permissions for accessing vaults in this client.
    pub fn with_default_vault_access(mut self, use_: bool, write: bool, clone_: bool) -> Self {
        self.use_vault_default = use_;
        self.write_vault_default = write;
        self.clone_vault_default = clone_;
        self
    }

    /// Set specific permissions for accessing the vault at `vault_path`.
    pub fn with_vault_access(mut self, vault_path: Vec<u8>, use_: bool, write: bool, clone_: bool) -> Self {
        self.use_vault_exceptions.insert(vault_path.clone(), use_);
        self.use_vault_exceptions.insert(vault_path.clone(), write);
        self.use_vault_exceptions.insert(vault_path, clone_);
        self
    }

    /// Set read and write permissions for the client's store.
    pub fn with_store_access(mut self, read: bool, write: bool) -> Self {
        self.read_store = read;
        self.write_store = write;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccessRequest {
    pub client_path: Vec<u8>,
    pub locations: Vec<Access>,
}

impl AccessRequest {
    pub fn check(&self, permissions: Permissions) -> bool {
        match permissions
            .exceptions
            .get(&self.client_path)
            .unwrap_or(&permissions.default)
        {
            Some(p) => self.check_with_permissions(p),
            None => false,
        }
    }

    fn check_with_permissions(&self, permissions: &ClientPermissions) -> bool {
        self.locations.iter().all(|access| match access {
            Access::Use { vault_path } => permissions
                .use_vault_exceptions
                .get(vault_path)
                .copied()
                .unwrap_or(permissions.use_vault_default),
            Access::Write { vault_path } => permissions
                .write_vault_exceptions
                .get(vault_path)
                .copied()
                .unwrap_or(permissions.write_vault_default),
            Access::Clone { vault_path } => permissions
                .clone_vault_exceptions
                .get(vault_path)
                .copied()
                .unwrap_or(permissions.clone_vault_default),
            Access::List { vault_path } => {
                let use_ = permissions
                    .use_vault_exceptions
                    .get(vault_path)
                    .copied()
                    .unwrap_or(permissions.use_vault_default);
                let write = permissions
                    .write_vault_exceptions
                    .get(vault_path)
                    .copied()
                    .unwrap_or(permissions.write_vault_default);
                let clone_ = permissions
                    .clone_vault_exceptions
                    .get(vault_path)
                    .copied()
                    .unwrap_or(permissions.clone_vault_default);
                use_ || write || clone_
            }
            Access::ReadStore => permissions.read_store,
            Access::WriteStore => permissions.write_store,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Access {
    Write { vault_path: Vec<u8> },
    Clone { vault_path: Vec<u8> },
    Use { vault_path: Vec<u8> },
    List { vault_path: Vec<u8> },
    ReadStore,
    WriteStore,
}

impl FwRequest<ShRequest> for AccessRequest {
    fn from_request(request: &ShRequest) -> Self {
        let client_path = request.client_path.clone();
        let locations = match &request.request {
            Request::CheckVault(CheckVault { vault_path }) | Request::ListIds(ListIds { vault_path }) => {
                vec![Access::List {
                    vault_path: vault_path.clone(),
                }]
            }
            Request::CheckRecord(CheckRecord { location }) => {
                vec![Access::List {
                    vault_path: location.vault_path().to_vec(),
                }]
            }
            #[cfg(test)]
            Request::ReadFromVault(ReadFromVault { location }) => {
                vec![Access::Clone {
                    vault_path: location.vault_path().to_vec(),
                }]
            }
            Request::WriteToRemoteVault(WriteToRemoteVault { location, .. })
            | Request::RevokeData(RevokeData { location }) => {
                vec![Access::Write {
                    vault_path: location.vault_path().to_vec(),
                }]
            }
            Request::ReadFromStore(ReadFromStore { .. }) => vec![Access::ReadStore],
            Request::WriteToStore(WriteToStore { .. }) | Request::DeleteFromStore(DeleteFromStore { .. }) => {
                vec![Access::WriteStore]
            }
            Request::Procedures(p) => p
                .procedures
                .iter()
                .flat_map(|proc| match proc {
                    StrongholdProcedure::RevokeData(procedures::RevokeData { location, .. }) => vec![Access::Write {
                        vault_path: location.vault_path().to_vec(),
                    }],
                    StrongholdProcedure::GarbageCollect(procedures::GarbageCollect { vault_path }) => {
                        vec![Access::Write {
                            vault_path: vault_path.clone(),
                        }]
                    }
                    proc => {
                        let mut access = Vec::new();
                        if let Some(input) = proc.input() {
                            access.push(Access::Use {
                                vault_path: input.vault_path().to_vec(),
                            });
                        }
                        if let Some(output) = proc.output() {
                            access.push(Access::Write {
                                vault_path: output.vault_path().to_vec(),
                            });
                        }
                        access
                    }
                })
                .collect(),
        };
        AccessRequest { client_path, locations }
    }
}
