use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::TimeZone;
use futures::{future, AsyncReadExt, Stream, StreamExt};
use libsignal_service::prelude::SessionStoreExt;
use libsignal_service::protocol::{ProtocolAddress, SessionStore};
use libsignal_service::provisioning::ProvisioningSecrets;
use libsignal_service::{
    attachment_cipher::decrypt_in_place,
    cipher,
    configuration::{ServiceConfiguration, SignalServers},
    content::{Content, ContentBody, Metadata},
    encrypt_device_name,
    groups_v2::{decrypt_group, GroupsManager, InMemoryCredentialsCache},
    messagepipe::{Incoming, MessagePipe, ServiceCredentials},
    prelude::{phonenumber::PhoneNumber, MessageSenderError, ProtobufMessage, Uuid},
    profile_cipher::ProfileCipher,
    proto::{
        data_message::Delete,
        sync_message::{
            self, sticker_pack_operation, Content as SyncContent, StickerPackOperation,
        },
        AttachmentPointer, DataMessage, EditMessage, GroupContextV2, NullMessage, SyncMessage,
        Verified,
    },
    protocol::{
        Aci, DeviceId, IdentityKeyStore, SenderCertificate, ServiceId, ServiceIdKind, Username,
    },
    provisioning::ProvisioningError,
    push_service::{PushService, ServiceIds, DEFAULT_DEVICE_ID},
    sender::{AttachmentSpec, AttachmentUploadError},
    sticker_cipher::derive_key,
    unidentified_access::UnidentifiedAccess,
    utils::TryIntoE164,
    websocket::{
        self,
        account::{AccountAttributes, DeviceCapabilities, DeviceInfo, WhoAmIResponse},
        SignalWebSocket,
    },
    zkgroup::{
        groups::{GroupMasterKey, GroupSecretParams},
        profiles::ProfileKey,
    },
    AccountManager, Profile, ServiceIdExt,
};
use libsignal_service::{
    libsignal_account_keys::AccountEntropyPool, proto::addressable_message::Author,
};
use rand::rng;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use tokio::sync::Mutex;
use tracing::{debug, error, info, trace, warn};
use url::Url;

use crate::model::contacts::Contact;
use crate::serde::serde_profile_key;
use crate::store::{ContentsStore, Sticker, StickerPack, StickerPackManifest, Store, Thread};
use crate::{model::groups::Group, AvatarBytes, Error, Manager};

pub use crate::model::messages::Received;

type ServiceCipher<S> = cipher::ServiceCipher<S>;
type MessageSender<S> = libsignal_service::prelude::MessageSender<S>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistrationType {
    Primary,
    Secondary,
}

/// Manager state when the client is registered and can send and receive messages from Signal
#[derive(Clone)]
pub struct Registered {
    pub(crate) identified_push_service: OnceLock<PushService>,
    pub(crate) unidentified_push_service: OnceLock<PushService>,
    pub(crate) identified_websocket: Arc<Mutex<Option<SignalWebSocket<websocket::Identified>>>>,
    pub(crate) unidentified_websocket: Arc<Mutex<Option<SignalWebSocket<websocket::Unidentified>>>>,
    pub(crate) unidentified_sender_certificate: Arc<Mutex<Option<SenderCertificate>>>,
    /// Held while a received message is decrypted, and by a caller while it sends: each reads a
    /// session, advances it and writes it back, and two doing so at once lose one of the changes,
    /// after which the other side cannot decrypt.
    pub(crate) sessions: Arc<tokio::sync::Mutex<()>>,

    pub(crate) data: Arc<RegistrationData>,
}

impl fmt::Debug for Registered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registered").finish_non_exhaustive()
    }
}

impl Registered {
    pub(crate) fn with_data(data: RegistrationData) -> Self {
        Self {
            identified_push_service: Default::default(),
            unidentified_push_service: Default::default(),
            identified_websocket: Default::default(),
            unidentified_websocket: Default::default(),
            unidentified_sender_certificate: Default::default(),
            sessions: Default::default(),
            data: Arc::new(data),
        }
    }

    fn servers(&self) -> SignalServers {
        self.data.signal_servers
    }

    fn service_configuration(&self) -> ServiceConfiguration {
        self.servers().into()
    }

    pub fn device_id(&self) -> DeviceId {
        self.data
            .device_id
            .and_then(|d| d.try_into().ok())
            .unwrap_or(*DEFAULT_DEVICE_ID)
    }

    pub(crate) fn identified_push_service(&self) -> PushService {
        self.identified_push_service
            .get_or_init(|| {
                PushService::new(self.servers(), Some(self.credentials()), crate::USER_AGENT)
            })
            .clone()
    }

    pub(crate) fn credentials(&self) -> ServiceCredentials {
        ServiceCredentials {
            aci: Some(self.data.service_ids.aci),
            pni: Some(self.data.service_ids.pni),
            phonenumber: (&self.data.phone_number)
                .try_into_e164()
                .expect("valid phone number"),
            password: Some(self.data.password.clone()),
            device_id: self.data.device_id.and_then(|d| d.try_into().ok()),
        }
    }
}

/// Registration data like device name, and credentials to connect to Signal
#[derive(Serialize, Deserialize, Clone)]
pub struct RegistrationData {
    pub signal_servers: SignalServers,
    pub device_name: Option<String>,
    pub phone_number: PhoneNumber,
    #[serde(flatten)]
    pub service_ids: ServiceIds,
    pub(crate) password: String,
    pub device_id: Option<u32>,
    pub registration_id: u32,
    #[serde(default)]
    pub pni_registration_id: Option<u32>,
    #[serde(with = "serde_profile_key")]
    pub(crate) profile_key: ProfileKey,
}

impl RegistrationData {
    /// Our own profile key
    pub fn profile_key(&self) -> ProfileKey {
        self.profile_key
    }

    /// The name of the device (if linked as secondary)
    pub fn device_name(&self) -> Option<&str> {
        self.device_name.as_deref()
    }
}

/// What [`Manager::sync_storage`] read.
#[derive(Default)]
pub struct AccountStorage {
    /// The version of the records, to pass to the next call.
    pub version: u64,
    /// Whether the records were read; false when they were still at the known version.
    pub changed: bool,
    /// The account's own record; absent when the account has not written one.
    pub account: Option<libsignal_service::proto::AccountRecord>,
    /// How many groups the account lists.
    pub groups: usize,
    /// The people the account lists: each one's identifier and the name the account's owner knows
    /// them by, "" when the record has none; and whether the owner has dealings with them: has
    /// accepted their messages or written to them, or has them in the phone's address book; and
    /// whether the name is one the owner gave (a nickname, or the address book's) and not the one
    /// the person chose for their profile.
    pub contacts: Vec<(Uuid, String, bool, bool)>,
}

impl<S: Store> Manager<S, Registered> {
    /// Loads a previously registered account from the implemented [Store].
    ///
    /// Returns a instance of [Manager] you can use to send & receive messages.
    pub async fn load_registered(store: S) -> Result<Self, Error<S::Error>> {
        let registration_data = store
            .load_registration_data()
            .await?
            .ok_or(Error::NotYetRegisteredError)?;

        let registered = Registered::with_data(registration_data);

        if let Some(sender_certificate) = store.sender_certificate().await? {
            registered
                .unidentified_sender_certificate
                .lock()
                .await
                .replace(sender_certificate);
        }

        Ok(Self {
            store,
            state: registered,
        })
    }

    /// Returns a handle to the [Store] implementation.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Returns a handle on the [RegistrationData].
    pub fn registration_data(&self) -> &RegistrationData {
        &self.state.data
    }

    /// Returns a clone of a cached push service (with credentials).
    ///
    /// If no service is yet cached, it will create and cache one.
    fn identified_push_service(&self) -> PushService {
        self.state.identified_push_service()
    }

    /// Returns a clone of a cached push service (without credentials).
    ///
    /// If no service is yet cached, it will create and cache one.
    fn unidentified_push_service(&self) -> PushService {
        self.state
            .unidentified_push_service
            .get_or_init(|| PushService::new(self.state.servers(), None, crate::USER_AGENT))
            .clone()
    }

    /// Returns the current identified websocket, or creates a new one
    ///
    /// A new one is created if the current websocket is closed, or if there is none yet.
    async fn identified_websocket(
        &self,
        require_unused: bool,
    ) -> Result<SignalWebSocket<websocket::Identified>, Error<S::Error>> {
        let mut identified_ws = self.state.identified_websocket.lock().await;
        match identified_ws
            .as_ref()
            .filter(|ws| !ws.is_closed())
            .filter(|ws| !(require_unused && ws.is_used()))
        {
            Some(ws) => Ok(ws.clone()),
            None => {
                let headers = &[("X-Signal-Receive-Stories", "false")];
                let ws = self
                    .identified_push_service()
                    .ws(
                        "/v1/websocket/",
                        "/v1/keepalive",
                        headers,
                        Some(self.credentials()),
                    )
                    .await?;
                identified_ws.replace(ws.clone());
                debug!("initialized identified websocket");

                Ok(ws)
            }
        }
    }

    /// Returns the current unidentified websocket, or creates a new one
    ///
    /// A new one is created if the current websocket is closed, or if there is none yet.
    async fn unidentified_websocket(
        &self,
    ) -> Result<SignalWebSocket<websocket::Unidentified>, Error<S::Error>> {
        let mut unidentified_ws = self.state.unidentified_websocket.lock().await;
        match unidentified_ws.as_ref().filter(|ws| !ws.is_closed()) {
            Some(ws) => Ok(ws.clone()),
            None => {
                let ws = self
                    .unidentified_push_service()
                    .ws("/v1/websocket/", "/v1/keepalive", &[], None)
                    .await?;
                unidentified_ws.replace(ws.clone());
                debug!("initialized unidentified websocket");

                Ok(ws)
            }
        }
    }

    /// Request the primary device to encrypt & send all of its contacts.
    ///
    /// **Note**: If successful, the contacts are not yet received and stored, but will only be
    /// processed when they're received after polling on the
    pub async fn request_contacts(&mut self) -> Result<(), Error<S::Error>> {
        self.request_sync(sync_message::request::Type::Contacts)
            .await
    }

    /// Asks the primary device for the account's settings, which arrive as a configuration sync
    /// message.
    pub async fn request_configuration(&mut self) -> Result<(), Error<S::Error>> {
        self.request_sync(sync_message::request::Type::Configuration)
            .await
    }

    async fn request_sync(
        &mut self,
        kind: sync_message::request::Type,
    ) -> Result<(), Error<S::Error>> {
        trace!(?kind, "requesting sync");
        let sync_message = SyncMessage {
            content: Some(SyncContent::Request(sync_message::Request {
                r#type: Some(kind.into()),
            })),
            ..SyncMessage::with_padding(&mut rand::rng())
        };

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64;

        self.send_message(self.state.data.service_ids.aci(), sync_message, timestamp)
            .await?;

        Ok(())
    }

    /// Tells the account's other devices that messages were read here, each named by its sender and
    /// the time it was sent.
    pub async fn sync_read(
        &mut self,
        messages: &[(ServiceId, u64)],
    ) -> Result<(), Error<S::Error>> {
        let read = messages
            .iter()
            .map(|(sender, timestamp)| sync_message::Read {
                sender_aci: Some(sender.service_id_string()),
                timestamp: Some(*timestamp),
                ..Default::default()
            })
            .collect();
        let sync_message = SyncMessage {
            read,
            ..SyncMessage::with_padding(&mut rand::rng())
        };

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64;

        self.send_message(self.state.data.service_ids.aci(), sync_message, timestamp)
            .await?;

        Ok(())
    }

    /// Tells the account's other devices that the owner accepted the messages of a person who had
    /// written unasked, so that they stop holding those messages apart too.
    pub async fn sync_request_accepted(&mut self, person: &ServiceId) -> Result<(), Error<S::Error>> {
        let response = sync_message::MessageRequestResponse {
            thread_aci: Some(person.service_id_string()),
            r#type: Some(sync_message::message_request_response::Type::Accept as i32),
            ..Default::default()
        };
        let sync_message = SyncMessage {
            content: Some(sync_message::Content::MessageRequestResponse(response)),
            ..SyncMessage::with_padding(&mut rand::rng())
        };
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64;
        self.send_message(self.state.data.service_ids.aci(), sync_message, timestamp)
            .await?;
        Ok(())
    }

    /// Reads the account's records on the storage service, where the account keeps what all its
    /// devices share: fetches and saves every group listed there that the store does not have, and
    /// returns the account's own record with how many groups are listed. Nothing is read when the
    /// records are still at `known_version`, the version a previous call returned. `Ok(None)` when
    /// this device has not been given the account's keys yet; they arrive from the primary device
    /// soon after linking.
    pub async fn sync_storage(
        &mut self,
        known_version: Option<u64>,
    ) -> Result<Option<AccountStorage>, Error<S::Error>> {
        use libsignal_service::master_key::{MasterKey, StorageServiceKey};
        use libsignal_service::proto::{manifest_record::identifier, storage_record::Record};

        let master_key = match self.account_entropy_pool().await? {
            Some(pool) => MasterKey {
                inner: pool.derive_svr_key(),
            },
            None => match self.store.fetch_master_key().await? {
                Some(key) => key,
                None => return Ok(None),
            },
        };
        let storage = libsignal_service::StorageService::new(
            self.identified_push_service(),
            StorageServiceKey::from_master_key(&master_key),
        )
        .await?;
        let manifest = match known_version {
            Some(version) => match storage.manifest_if_changed(version).await? {
                Some(manifest) => manifest,
                None => {
                    return Ok(Some(AccountStorage {
                        version,
                        ..Default::default()
                    }))
                }
            },
            None => storage.manifest().await?,
        };
        let record_ikm = (!manifest.record_ikm.is_empty()).then_some(&manifest.record_ikm[..]);
        let keys: Vec<Vec<u8>> = manifest
            .identifiers
            .iter()
            .filter(|i| matches!(i.r#type(), identifier::Type::Groupv2 | identifier::Type::Account | identifier::Type::Contact))
            .map(|i| i.raw.clone())
            .collect();

        let mut read = AccountStorage {
            version: manifest.version,
            changed: true,
            ..Default::default()
        };
        let mut groups_manager = Box::pin(self.groups_manager()).await?;
        // (the service answers for at most 1,000 keys at a time)
        for keys in keys.chunks(1000) {
            for item in storage.read_items(keys.to_vec(), record_ikm).await? {
                match item.record {
                    Some(Record::Account(record)) => read.account = Some(record),
                    // A person's record carries the names the owner gave them, the name from the
                    // phone's address book and the one they chose themselves, taken in that order,
                    // and the key their profile is read with.
                    Some(Record::Contact(contact)) => {
                        let Ok(aci) = Uuid::parse_str(&contact.aci) else { continue };
                        let join = |given: &str, family: &str| [given, family].iter().filter(|p| !p.is_empty()).copied().collect::<Vec<_>>().join(" ");
                        let nickname = contact.nickname.as_ref().map(|n| join(&n.given, &n.family)).unwrap_or_default();
                        let names = [nickname, join(&contact.system_given_name, &contact.system_family_name), contact.system_nickname.clone(), join(&contact.given_name, &contact.family_name)];
                        if let Ok(key) = <[u8; 32]>::try_from(&contact.profile_key[..]) {
                            self.store.upsert_profile_key(&aci, ProfileKey::create(key)).await?;
                        }
                        let known = contact.whitelisted || !contact.system_given_name.is_empty() || !contact.system_family_name.is_empty() || !contact.system_nickname.is_empty();
                        let given_by_owner = names[..3].iter().any(|n| !n.is_empty());
                        read.contacts.push((aci, names.into_iter().find(|n| !n.is_empty()).unwrap_or_default(), known, given_by_owner));
                    }
                    Some(Record::GroupV2(group)) if group.master_key.len() == 32 => {
                        read.groups += 1;
                        upsert_group(&self.store, &mut groups_manager, &group.master_key, &0)
                            .await?;
                    }
                    _ => {}
                }
            }
        }
        Ok(Some(read))
    }

    async fn sender_certificate(&self) -> Result<SenderCertificate, Error<S::Error>> {
        let needs_renewal = |sender_certificate: Option<&SenderCertificate>| -> bool {
            if sender_certificate.is_none() {
                return true;
            }

            let seconds_since_epoch = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards")
                .as_secs();

            if let Some(expiration) = sender_certificate.and_then(|s| s.expiration().ok()) {
                expiration.epoch_millis() / 1000 <= seconds_since_epoch + 600
            } else {
                true
            }
        };

        let mut unidentified_sender_certificate =
            self.state.unidentified_sender_certificate.lock().await;
        if needs_renewal(unidentified_sender_certificate.as_ref()) {
            let sender_certificate = self
                .identified_websocket(false)
                .await?
                .get_uuid_only_sender_certificate()
                .await?;
            self.store
                .save_sender_certificate(&sender_certificate)
                .await?;
            unidentified_sender_certificate.replace(sender_certificate);
        }

        Ok(unidentified_sender_certificate
            .clone()
            .expect("logic error"))
    }

    async fn account_entropy_pool(&self) -> Result<Option<AccountEntropyPool>, Error<S::Error>> {
        let from_store = self.store().fetch_account_entropy_pool().await?;

        if let Some(key) = from_store {
            Ok(Some(key))
        } else if self.registration_type() == RegistrationType::Primary {
            let key = AccountEntropyPool::generate(&mut rand::rng());
            self.store().store_account_entropy_pool(Some(&key)).await?;
            Ok(Some(key))
        } else {
            Ok(None)
        }
    }

    pub async fn submit_recaptcha_challenge(
        &self,
        token: &str,
        captcha: &str,
    ) -> Result<(), Error<S::Error>> {
        let mut account_manager = AccountManager::new(
            self.identified_push_service(),
            self.identified_websocket(false).await?,
            None,
        );
        account_manager
            .submit_recaptcha_challenge(token, captcha)
            .await?;
        Ok(())
    }

    /// Fetches basic information on the registered device.
    pub async fn whoami(&self) -> Result<WhoAmIResponse, Error<S::Error>> {
        Ok(self.identified_websocket(false).await?.whoami().await?)
    }

    pub fn device_id(&self) -> DeviceId {
        self.state.device_id()
    }

    /// Fetches the profile (name, about, status emoji) of the registered user.
    pub async fn retrieve_profile(&mut self) -> Result<Profile, Error<S::Error>> {
        self.retrieve_profile_by_uuid(self.state.data.service_ids.aci, self.state.data.profile_key)
            .await
    }

    /// Fetches the profile of the provided user by UUID and profile key.
    pub async fn retrieve_profile_by_uuid(
        &mut self,
        aci: impl Into<Aci>,
        profile_key: ProfileKey,
    ) -> Result<Profile, Error<S::Error>> {
        let aci = aci.into();

        // Check if profile is cached.
        // TODO: Create a migration in the store removing all profiles.
        // TODO: Is there some way to know if this is outdated?
        if let Some(profile) = self
            .store
            .profile(aci.into(), profile_key)
            .await
            .ok()
            .flatten()
        {
            return Ok(profile);
        }

        let mut account_manager = AccountManager::new(
            self.identified_push_service(),
            self.identified_websocket(false).await?,
            Some(profile_key),
        );

        let profile = account_manager.retrieve_profile(aci).await?;

        let _ = self
            .store
            .save_profile(aci.into(), profile_key, profile.clone())
            .await;
        Ok(profile)
    }

    /// Updates the user's profile information.
    pub async fn update_profile(
        &mut self,
        name: libsignal_service::profile_name::ProfileName<String>,
        about: Option<String>,
        emoji: Option<String>,
    ) -> Result<(), Error<S::Error>> {
        let aci = self.state.data.service_ids.aci();
        let mut account_manager = AccountManager::new(
            self.identified_push_service(),
            self.identified_websocket(false).await?,
            Some(self.state.data.profile_key),
        );

        account_manager
            .upload_versioned_profile_without_avatar::<_, String>(
                aci,
                name,
                about,
                emoji,
                true, // retain_avatar
                &mut rand::rng(),
            )
            .await?;

        // Retrieve and save locally so we have the updated version
        let profile = account_manager.retrieve_profile(aci).await?;
        let _ = self
            .store
            .save_profile(aci.into(), self.state.data.profile_key, profile)
            .await;

        Ok(())
    }

    pub async fn retrieve_group_avatar(
        &mut self,
        context: GroupContextV2,
    ) -> Result<Option<AvatarBytes>, Error<S::Error>> {
        let master_key_bytes = context
            .master_key()
            .try_into()
            .expect("Master key bytes to be of size 32.");

        // Check if group avatar is cached.
        // TODO: Is there some way to know if this is outdated?
        if let Some(avatar) = self
            .store
            .group_avatar(master_key_bytes)
            .await
            .ok()
            .flatten()
        {
            return Ok(Some(avatar));
        }

        let mut gm = Box::pin(self.groups_manager()).await?;
        let Some(group) = upsert_group(
            &self.store,
            &mut gm,
            context.master_key(),
            &context.revision(),
        )
        .await?
        else {
            return Ok(None);
        };

        // Empty path means no avatar was set.
        if group.avatar.is_empty() {
            return Ok(None);
        }

        let avatar = gm
            .retrieve_avatar(
                &group.avatar,
                GroupSecretParams::derive_from_master_key(GroupMasterKey::new(master_key_bytes)),
            )
            .await?;
        if let Some(avatar) = &avatar {
            let _ = self.store.save_group_avatar(master_key_bytes, avatar).await;
        }
        Ok(avatar)
    }

    pub async fn retrieve_profile_avatar_by_uuid(
        &mut self,
        uuid: Uuid,
        profile_key: ProfileKey,
    ) -> Result<Option<AvatarBytes>, Error<S::Error>> {
        // Check if profile avatar is cached.
        // TODO: Is there some way to know if this is outdated?
        if let Some(avatar) = self
            .store
            .profile_avatar(uuid, profile_key)
            .await
            .ok()
            .flatten()
        {
            return Ok(Some(avatar));
        }

        let profile =
            if let Some(profile) = self.store.profile(uuid, profile_key).await.ok().flatten() {
                profile
            } else {
                self.retrieve_profile_by_uuid(uuid, profile_key).await?
            };

        let Some(avatar) = profile.avatar.as_ref() else {
            return Ok(None);
        };

        let mut websocket = self.unidentified_websocket().await?;

        let mut avatar_stream = websocket.retrieve_profile_avatar(avatar).await?;
        // 10MB is what Signal Android allocates
        let mut contents = Vec::with_capacity(10 * 1024 * 1024);
        let len = avatar_stream.read_to_end(&mut contents).await?;
        contents.truncate(len);

        let cipher = ProfileCipher::new(profile_key);

        let avatar = cipher.decrypt_avatar(&contents)?;
        let _ = self
            .store
            .save_profile_avatar(uuid, profile_key, &avatar)
            .await;
        Ok(Some(avatar))
    }

    /// Where the service keeps a person's profile picture now ("" when they have none), and the
    /// name their profile gives ("" when it gives none). The service is asked each time (one small
    /// request) and the answer replaces the kept profile, so a caller that remembers the address
    /// knows from it alone whether the picture changed.
    pub async fn profile_avatar_path(
        &mut self,
        uuid: Uuid,
        profile_key: ProfileKey,
    ) -> Result<(String, String), Error<S::Error>> {
        let mut account_manager = AccountManager::new(
            self.identified_push_service(),
            self.identified_websocket(false).await?,
            Some(profile_key),
        );
        let profile = account_manager.retrieve_profile(uuid.into()).await?;
        let path = profile.avatar.clone().unwrap_or_default();
        let name = profile.name.as_ref().map(|n| n.to_string()).unwrap_or_default();
        let _ = self.store.save_profile(uuid, profile_key, profile).await;
        Ok((path, name))
    }

    /// Fetches the profile picture at an address `profile_avatar_path` gave, and keeps it.
    pub async fn fetch_profile_avatar(
        &mut self,
        uuid: Uuid,
        profile_key: ProfileKey,
        path: &str,
    ) -> Result<AvatarBytes, Error<S::Error>> {
        let mut websocket = self.unidentified_websocket().await?;
        let mut avatar_stream = websocket.retrieve_profile_avatar(path).await?;
        let mut contents = Vec::new();
        avatar_stream.read_to_end(&mut contents).await?;
        let avatar = ProfileCipher::new(profile_key).decrypt_avatar(&contents)?;
        let _ = self
            .store
            .save_profile_avatar(uuid, profile_key, &avatar)
            .await;
        Ok(avatar)
    }

    /// Where the service keeps a group's picture, as the group's kept state says ("" for none);
    /// None for a group that is not kept yet. The state is brought up to date as the group's
    /// messages arrive, so this asks the service nothing.
    pub async fn group_avatar_path(
        &self,
        master_key_bytes: [u8; 32],
    ) -> Result<Option<String>, Error<S::Error>> {
        Ok(self.store.group(master_key_bytes).await?.map(|group| group.avatar))
    }

    /// Whether the account may change who is in a group: it is one of the group's administrators,
    /// as the group's kept state says. False for a group not kept yet.
    pub async fn group_admin(&self, master_key_bytes: [u8; 32]) -> Result<bool, Error<S::Error>> {
        let own = self.state.data.service_ids.aci();
        Ok(self.store.group(master_key_bytes).await?.is_some_and(|group| {
            group.members.iter().any(|m| m.aci == own && m.role == libsignal_service::groups_v2::Role::Administrator)
        }))
    }

    /// Removes a member from a group, as one of its administrators: the change is made on the
    /// service, the group's kept state is brought up to it, and the group's members, the removed
    /// one among them, are sent the change so their programs show it at once. The service refuses
    /// the change from an account that is not an administrator, or one made against a revision
    /// that is no longer the newest.
    pub async fn remove_group_member(
        &mut self,
        master_key_bytes: [u8; 32],
        member: Aci,
    ) -> Result<(), Error<S::Error>> {
        use libsignal_service::proto::{group_change, GroupChangeResponse};

        let mut gm = Box::pin(self.groups_manager()).await?;
        let Some(group) = upsert_group(&self.store, &mut gm, &master_key_bytes, &0).await? else {
            return Err(Error::UnknownGroup);
        };
        let secret = GroupSecretParams::derive_from_master_key(GroupMasterKey::new(master_key_bytes));
        let deleted = libsignal_service::zkgroup::serialize(&secret.encrypt_service_id(ServiceId::Aci(member)));
        let revision = group.revision + 1;
        let actions = group_change::Actions {
            version: revision,
            delete_members: vec![group_change::actions::DeleteMemberAction { deleted_user_id: deleted }],
            ..Default::default()
        };
        let auth = gm.get_authorization_for_today(&mut rng(), secret).await?;
        let response = self
            .identified_push_service()
            .request(
                reqwest::Method::PATCH,
                libsignal_service::configuration::Endpoint::storage("/v2/groups/"),
                libsignal_service::push_service::HttpAuthOverride::Identified(auth),
            )?
            .header(reqwest::header::CONTENT_TYPE, "application/x-protobuf")
            .body(actions.encode_to_vec())
            .send()
            .await
            .map_err(|e| libsignal_service::prelude::ServiceError::SendError { reason: e.to_string() })?;
        let status = response.status();
        let body = response.bytes().await.map_err(|e| libsignal_service::prelude::ServiceError::SendError { reason: e.to_string() })?;
        if !status.is_success() {
            return Err(libsignal_service::prelude::ServiceError::UnhandledResponseCode { status, body: String::from_utf8_lossy(&body).into_owned() }.into());
        }
        let change = GroupChangeResponse::decode(body)
            .map_err(|_| libsignal_service::prelude::ServiceError::InvalidFrame { reason: "a group change response that does not decode" })?
            .group_change
            .unwrap_or_default();
        upsert_group(&self.store, &mut gm, &master_key_bytes, &revision).await?;

        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
        let update = DataMessage {
            group_v2: Some(GroupContextV2 {
                master_key: Some(master_key_bytes.to_vec()),
                revision: Some(revision),
                group_change: Some(change.encode_to_vec()),
                ..Default::default()
            }),
            timestamp: Some(timestamp),
            ..Default::default()
        };
        // (the remaining members, from the state just brought up to date, and the one removed)
        self.send_message_to_group(&master_key_bytes, update.clone(), timestamp).await?;
        if let Err(error) = self.send_message(member, update, timestamp).await {
            warn!(%error, "the removed member could not be sent the change");
        }
        Ok(())
    }

    /// Fetches the group picture at an address `group_avatar_path` gave, and keeps it.
    pub async fn fetch_group_avatar(
        &mut self,
        master_key_bytes: [u8; 32],
        path: &str,
    ) -> Result<Option<AvatarBytes>, Error<S::Error>> {
        let mut gm = Box::pin(self.groups_manager()).await?;
        let avatar = gm
            .retrieve_avatar(
                path,
                GroupSecretParams::derive_from_master_key(GroupMasterKey::new(master_key_bytes)),
            )
            .await?;
        if let Some(avatar) = &avatar {
            let _ = self.store.save_group_avatar(master_key_bytes, avatar).await;
        }
        Ok(avatar)
    }

    async fn groups_manager(
        &self,
    ) -> Result<GroupsManager<InMemoryCredentialsCache>, Error<S::Error>> {
        let service_configuration = self.state.service_configuration();
        let server_public_params = service_configuration.zkgroup_server_public_params;

        let groups_credentials_cache = InMemoryCredentialsCache::default();
        let groups_manager = GroupsManager::new(
            self.state.data.service_ids.clone(),
            self.identified_push_service(),
            self.unidentified_websocket().await?,
            groups_credentials_cache,
            server_public_params,
        );

        Ok(groups_manager)
    }

    /// Starts receiving and storing messages.
    ///
    /// As a client, it is heavily recommended to process incoming messages and wait for the `Received::QueueEmpty` messages
    /// until giving the ability for users to send messages. That way, all possible updates (sessions, profile keys, sender keys)
    /// are processed _before_ trying to encrypt and send messages, which might get rejected by recipients otherwise.
    ///
    /// Returns a [futures::Stream] of messages to consume. Messages will also be stored by the implementation of the [Store].
    /// The lock a caller holds around its sends while a stream from `receive_messages` is being
    /// read, so that a send and the decryption of a received message do not overlap. The stream
    /// takes it for each message; it must be read by a task of its own, not by one that sends.
    pub fn sessions(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.state.sessions.clone()
    }

    pub async fn receive_messages(
        &mut self,
    ) -> Result<impl Stream<Item = Received>, Error<S::Error>> {
        struct StreamState<Receiver, Store, AciStore, PniStore> {
            sessions: Arc<tokio::sync::Mutex<()>>,
            store: Store,
            identified_websocket: SignalWebSocket<websocket::Identified>,
            unidentified_websocket: SignalWebSocket<websocket::Unidentified>,
            encrypted_messages: Receiver,
            push_service: PushService,
            service_cipher_aci: ServiceCipher<AciStore>,
            service_cipher_pni: ServiceCipher<PniStore>,
            groups_manager: GroupsManager<InMemoryCredentialsCache>,
            service_ids: ServiceIds,
            message_sender: MessageSender<AciStore>,
            account_entropy_pool: Option<AccountEntropyPool>,
            registration_type: RegistrationType,
        }

        let identified_push_service = self.identified_push_service();
        // NB: here, we initialise a *fresh* Signal websocket, which means any other use of the previous one will go into nirvana
        let identified_websocket = self.identified_websocket(true).await?;

        let mut account_manager = AccountManager::new(
            identified_push_service.clone(),
            identified_websocket.clone(),
            None,
        );

        let store_inner = self.store.clone();
        let registration_data_inner = self.registration_data().clone();

        // We make a task to update the account attributes and refresh pre keys as needed that will
        // only yield a value if one of the two operations fail (stop signal).
        //
        // This is necessary because in this context, we can't do the classic tokio::spawn with a
        // oneshot::channel() or CancellationToken because of !Send constraints in the Store.
        let refresh_registration_task = async move {
            if let Err(error) =
                set_account_attributes(&mut account_manager, &store_inner, &registration_data_inner)
                    .await
            {
                error!(%error, "failed to set account attributes, this is problematic and should never happen!");
            }

            if let Err(error) = register_pre_keys(&store_inner, &mut account_manager).await {
                error!(%error, "failed to register pre-keys, this is problematic and should never happen!");
            }

            // Never return, which keeps the messages stream alive.
            future::pending::<()>().await
        };

        let encrypted_messages = MessagePipe::from_socket(identified_websocket.clone());

        let init = StreamState {
            sessions: self.state.sessions.clone(),
            store: self.store.clone(),
            identified_websocket,
            unidentified_websocket: self.unidentified_websocket().await?,
            encrypted_messages: Box::pin(encrypted_messages.stream()),
            push_service: identified_push_service,
            service_cipher_aci: self.new_service_cipher_aci(),
            service_cipher_pni: self.new_service_cipher_pni(),
            groups_manager: Box::pin(self.groups_manager()).await?,
            service_ids: self.state.data.service_ids.clone(),
            message_sender: self.new_message_sender().await?,
            account_entropy_pool: self.account_entropy_pool().await?,
            registration_type: self.registration_type(),
        };

        debug!("starting to consume incoming message stream");

        let incoming_messages_stream = futures::stream::unfold(init, |mut state| {
            async move {
                loop {
                    match state.encrypted_messages.next().await {
                        Some(Ok(Incoming::Envelope(envelope))) => {
                            // A message sent without sealing its sender has its delivery reported by
                            // the service itself, in an envelope with nothing to decrypt, addressed to
                            // the device that sent it. It is handed on as the receipt it is: opening it
                            // like a message finds no content and would drop it.
                            if envelope.is_receipt() {
                                let (Some(sender), Some(destination), Some(sent)) = (
                                    envelope.parse_source_service_id(),
                                    envelope.parse_destination_service_id(),
                                    envelope.client_timestamp,
                                ) else {
                                    continue;
                                };
                                let Ok(sender_device) = DeviceId::try_from(envelope.source_device_id()) else {
                                    continue;
                                };
                                let at = |ms: u64| chrono::Utc.timestamp_millis_opt(ms as i64).single().unwrap_or_default();
                                let receipt = libsignal_service::proto::ReceiptMessage {
                                    r#type: Some(libsignal_service::proto::receipt_message::Type::Delivery as i32),
                                    timestamp: vec![sent],
                                };
                                let content = Content {
                                    metadata: Metadata {
                                        sender,
                                        destination,
                                        sender_device,
                                        pni_verified: None,
                                        client_timestamp: at(sent),
                                        server_timestamp: at(envelope.server_timestamp()),
                                        needs_receipt: false,
                                        unidentified_sender: false,
                                        was_plaintext: false,
                                        server_guid: None,
                                    },
                                    body: ContentBody::ReceiptMessage(receipt),
                                };
                                return Some((Received::Content(Box::new(content)), state));
                            }
                            let _sessions = state.sessions.clone().lock_owned().await;
                            // (when the message was sent, by which its sender knows it, for asking for it again)
                            let sent = envelope.client_timestamp();
                            let envelope = {
                                // the permit is released at the end of the block (impl Drop)
                                match envelope.parse_destination_service_id() {
                                    None => {
                                        warn!("Message has no destination service ID, dropping.");
                                        continue;
                                    }
                                    Some(ServiceId::Aci(_)) => {
                                        state
                                            .service_cipher_aci
                                            .open_envelope(envelope, &mut rand::rng())
                                            .await
                                    }
                                    Some(ServiceId::Pni(pni)) => {
                                        if pni == state.service_ids.pni()
                                            && envelope.parse_source_service_id().is_none()
                                        {
                                            warn!("Got a sealed sender message to our PNI? Invalid message, ignoring.");
                                            continue;
                                        }
                                        state
                                            .service_cipher_pni
                                            .open_envelope(envelope, &mut rng())
                                            .await
                                    }
                                }
                            };
                            match envelope {
                                Ok(Some(content)) => {
                                    if let ContentBody::DecryptionErrorMessage(e) = &content.body {
                                        warn!(
                                            error = ?e,
                                            "the sender could not decrypt a message"
                                        );
                                        return Some((
                                            Received::RetryRequest {
                                                sender: content.metadata.sender,
                                                timestamp: e.timestamp(),
                                            },
                                            state,
                                        ));
                                    }

                                    if let ContentBody::SynchronizeMessage(SyncMessage {
                                        content: Some(SyncContent::Request(request)),
                                        ..
                                    }) = &content.body
                                    {
                                        use libsignal_service::content::sync_message::request::Type as RequestType;

                                        match request.r#type() {
                                            RequestType::Contacts => {
                                                let contacts = state
                                                    .store
                                                    .contacts()
                                                    .await
                                                    .map(|i| {
                                                        i.collect::<Result<Vec<_>, _>>()
                                                            .unwrap_or_default()
                                                    })
                                                    .unwrap_or_default();

                                                let mut message_sender =
                                                    state.message_sender.clone();
                                                let aci = state.service_ids.aci();
                                                tokio::task::spawn_local(async move {
                                                    let result = message_sender
                                                    .send_contact_details(
                                                        &ServiceId::Aci(aci),
                                                        None,
                                                        contacts.into_iter().map(|c| libsignal_service::sender::ContactDetails {
                                                            number: c.phone_number.map(|p| p.to_string()),
                                                            aci: Some(c.uuid.to_string()),
                                                            aci_binary: Some(c.uuid.into_bytes().into()),
                                                            name: Some(c.name),
                                                            avatar: c.avatar.map(|a| libsignal_service::proto::contact_details::Avatar {
                                                                content_type: Some(a.content_type),
                                                                length: a.reader.len().try_into().ok(),
                                                            }),
                                                            expire_timer: Some(c.expire_timer),
                                                            expire_timer_version: Some(c.expire_timer_version),
                                                            inbox_position: None,
                                                        }),
                                                        false,
                                                        true,
                                                    )
                                                    .await;

                                                    if let Err(error) = result {
                                                        warn!(%error, "Error sending contact details to other devices");
                                                    }
                                                });
                                            }
                                            RequestType::Keys => {
                                                let mut message_sender =
                                                    state.message_sender.clone();
                                                let account_entropy_pool = state
                                                    .account_entropy_pool
                                                    .as_ref()
                                                    .map(|aep| aep.to_string());
                                                tokio::task::spawn_local(async move {
                                                    let result = message_sender.send_sync_message(SyncMessage {
                                                        content: Some(SyncContent::Keys(libsignal_service::content::sync_message::Keys {
                                                            account_entropy_pool,
                                                            media_root_backup_key: None,
                                                        })),
                                                        ..SyncMessage::with_padding(&mut rand::rng())
                                                    }).await;

                                                    if let Err(error) = result {
                                                        warn!(%error, "Error sending keys to other devices");
                                                    }
                                                });
                                            }
                                            RequestType::Blocked => {
                                                warn!("storing blocked user is not implemented yet! we will not report blocked users to the device requesting the sync.");
                                                let mut message_sender =
                                                    state.message_sender.clone();
                                                tokio::task::spawn_local(async move {
                                                    let result = message_sender.send_sync_message(SyncMessage {
                                                    content: Some(SyncContent::Blocked(libsignal_service::content::sync_message::Blocked {
                                                        numbers: vec![],
                                                        acis: vec![],
                                                        acis_binary: vec![],
                                                        group_ids: vec![],
                                                    })),
                                                    ..SyncMessage::with_padding(&mut rand::rng())
                                                }).await;

                                                    if let Err(error) = result {
                                                        warn!(%error, "Error sending blocked contacts to other devices");
                                                    }
                                                });
                                            }
                                            t => {
                                                info!(type = ?t, "Got sync request of currently unhandled type")
                                            }
                                        }
                                    }

                                    // contacts synchronization sent from the primary device (happens after linking, or on demand)
                                    if let ContentBody::SynchronizeMessage(SyncMessage {
                                        content: Some(SyncContent::Contacts(contacts)),
                                        ..
                                    }) = &content.body
                                    {
                                        match retrieve_contacts(&mut state.push_service, contacts).await {
                                            Ok(contacts) => {
                                                info!("saving contacts");
                                                for contact in contacts {
                                                    if let Err(error) = state
                                                        .store
                                                        .save_contact(&contact.into())
                                                        .await
                                                    {
                                                        warn!(%error, "failed to save contacts");
                                                        break;
                                                    }
                                                }
                                            }
                                            Err(error) => {
                                                warn!(%error, "failed to retrieve contacts");
                                            }
                                        }

                                        return Some((Received::Contacts, state));
                                    }

                                    // sticker pack operations
                                    if let ContentBody::SynchronizeMessage(SyncMessage {
                                        sticker_pack_operation,
                                        ..
                                    }) = &content.body
                                    {
                                        for operation in sticker_pack_operation {
                                            match operation.r#type() {
                                                sticker_pack_operation::Type::Install => {
                                                    let store = state.store.clone();
                                                    let unidentified_websocket =
                                                        state.unidentified_websocket.clone();
                                                    let operation = operation.clone();

                                                    // download stickers in the background
                                                    tokio::spawn(async move {
                                                        match download_sticker_pack(
                                                            store,
                                                            unidentified_websocket,
                                                            &operation,
                                                        )
                                                        .await
                                                        {
                                                            Ok(sticker_pack) => {
                                                                debug!(
                                                                "downloaded sticker pack: {} made by {}",
                                                                sticker_pack.manifest.title,
                                                                sticker_pack.manifest.author
                                                            );
                                                            }
                                                            Err(error) => error!(
                                                                %error,
                                                                "failed to download sticker pack"
                                                            ),
                                                        }
                                                    });
                                                }
                                                sticker_pack_operation::Type::Remove => match state
                                                    .store
                                                    .remove_sticker_pack(operation.pack_id())
                                                    .await
                                                {
                                                    Ok(was_present) => {
                                                        debug!(was_present, "removed stick pack")
                                                    }
                                                    Err(error) => {
                                                        error!(
                                                            %error,
                                                            "failed to remove sticker pack"
                                                        )
                                                    }
                                                },
                                            }
                                        }
                                    }

                                    // key synchronization sent from the primary device
                                    if let ContentBody::SynchronizeMessage(SyncMessage {
                                        content: Some(SyncContent::Keys(keys)),
                                        ..
                                    }) = &content.body
                                    {
                                        debug!("received key sync message");
                                        if state.registration_type == RegistrationType::Primary {
                                            warn!("received a key sync message as a primary device; ignoring")
                                        } else {
                                            match keys
                                                .account_entropy_pool
                                                .as_ref()
                                                .map(|s| AccountEntropyPool::from_str(s))
                                            {
                                                Some(Ok(aep)) => {
                                                    if let Err(error) = state
                                                        .store
                                                        .store_account_entropy_pool(Some(&aep))
                                                        .await
                                                    {
                                                        error!(%error, "failed to store account entropy pool");
                                                    }
                                                    state.account_entropy_pool = Some(aep);
                                                }
                                                Some(Err(error)) => {
                                                    warn!(%error, "cannot convert account entropy pool from string")
                                                }
                                                None => {}
                                            }
                                        }
                                    }

                                    // group update
                                    if let ContentBody::DataMessage(DataMessage {
                                        group_v2:
                                            Some(GroupContextV2 {
                                                master_key: Some(master_key_bytes),
                                                revision: Some(revision),
                                                ..
                                            }),
                                        ..
                                    })
                                    | ContentBody::SynchronizeMessage(SyncMessage {
                                        content:
                                            Some(SyncContent::Sent(sync_message::Sent {
                                                message:
                                                    Some(DataMessage {
                                                        group_v2:
                                                            Some(GroupContextV2 {
                                                                master_key: Some(master_key_bytes),
                                                                revision: Some(revision),
                                                                ..
                                                            }),
                                                        ..
                                                    }),
                                                ..
                                            })),
                                        ..
                                    }) = &content.body
                                    {
                                        // there's two things to implement: the group metadata (fetched from HTTP API)
                                        // and the group changes, which are part of the protobuf messages
                                        // this means we kinda need our own internal representation of groups inside of presage?
                                        if let Ok(Some(group)) = upsert_group(
                                            &state.store,
                                            &mut state.groups_manager,
                                            master_key_bytes,
                                            revision,
                                        )
                                        .await
                                        {
                                            trace!(?group, "upserted group");
                                        }
                                    }

                                    if let Err(error) = save_message(
                                        &mut state.store,
                                        &mut state.identified_websocket,
                                        content.clone(),
                                        None,
                                    )
                                    .await
                                    {
                                        error!(%error, "error saving message to store");
                                    }

                                    return Some((Received::Content(Box::new(content)), state));
                                }
                                Ok(None) => {
                                    debug!("empty envelope, message will be skipped!")
                                }
                                Err(error) => {
                                    error!(%error, "error opening envelope, message will be skipped!");
                                    if let libsignal_service::prelude::ServiceError::SealedSenderDecryptionError(libsignal_service::cipher::SealedSenderDecryptionError { sender: Some(sender), .. }) = &error {
                                        let device = sender.device_id();
                                        if let Some(sender) = ServiceId::parse_from_service_id_string(sender.name()) {
                                            return Some((Received::DecryptionError { sender, device, timestamp: sent }, state));
                                        }
                                    }
                                }
                            }
                        }
                        Some(Ok(Incoming::QueueEmpty)) => {
                            debug!("got empty queue");
                            if state.account_entropy_pool.is_none() {
                                debug!("device does not have the needed keys; requesting from primary device");

                                let mut message_sender = state.message_sender.clone();
                                tokio::task::spawn_local(async move {
                                    let result = message_sender
                                        .send_sync_message(SyncMessage {
                                            content: Some(SyncContent::Request(
                                                sync_message::Request {
                                                    r#type: Some(
                                                        sync_message::request::Type::Keys.into(),
                                                    ),
                                                },
                                            )),
                                            ..SyncMessage::with_padding(&mut rand::rng())
                                        })
                                        .await;

                                    if let Err(error) = result {
                                        warn!(%error, "Error sending blocked contacts to other devices");
                                    }
                                });
                            }
                            return Some((Received::QueueEmpty, state));
                        }
                        Some(Err(error)) => {
                            error!(%error, "unexpected error in message receiving loop")
                        }
                        None => return None,
                    }
                }
            }
        });

        Ok(Box::pin(
            // We use the returning of the async closure in take_until as a stop signal
            // if the future resolves *anything* the stream will end.
            incoming_messages_stream.take_until(refresh_registration_task),
        ))
    }

    /// Uses Signal's SGX contact discovery service to resolve a phone number to its matching account identity
    #[cfg(feature = "cdsi")]
    pub async fn discover_contacts_by_phone_number<P: TryIntoE164>(
        &mut self,
        phone_numbers: impl IntoIterator<Item = P>,
    ) -> Result<Vec<(PhoneNumber, Option<ServiceId>)>, Error<S::Error>> {
        use libsignal_service::websocket::directory::LookupRequest;

        let mut ws = self.identified_websocket(false).await?;

        let lookup_request = LookupRequest {
            new_e164s: phone_numbers
                .into_iter()
                .filter_map(|p| p.try_into_e164().ok())
                .collect(),
            ..Default::default()
        };

        Ok(ws
            .discover_contacts(lookup_request)
            .await?
            .into_iter()
            .map(|(e164, service_id)| {
                use libsignal_service::utils::phonenumber_from_signal;
                (phonenumber_from_signal(&e164), service_id)
            })
            .collect())
    }

    /// Resolves a username (which has a text part and an additional random number) to its account identity
    /// for sending messages.
    pub async fn lookup_username(
        &mut self,
        username: &str,
    ) -> Result<Option<Aci>, Error<S::Error>> {
        let username = Username::new(username)?;
        let mut ws = self.unidentified_websocket().await?;
        let resolved_username = ws.look_up_username(&username).await?;
        Ok(resolved_username)
    }

    /// Sends a messages to the provided [ServiceId].
    /// The timestamp should be set to now and is used by Signal mobile apps
    /// to order messages later, and apply reactions.
    ///
    /// This method will automatically update the [DataMessage::expire_timer] if it is set to
    /// [None] such that the chat will keep the current expire timer. If the expire timer is set,
    /// it will be used as is, and the expire timer version will be incremented.
    pub async fn send_message(
        &mut self,
        recipient: impl Into<ServiceId>,
        message: impl Into<ContentBody>,
        timestamp: u64,
    ) -> Result<(), Error<S::Error>> {
        let mut sender = self.new_message_sender().await?;
        let recipient = recipient.into();

        // TODO: Populate this flag based on the recipient information
        //
        // Issue <https://github.com/whisperfish/presage/issues/252>
        let include_pni_signature = false;
        let thread = Thread::Contact(recipient);
        let mut content_body: ContentBody = message.into();
        // A typing message is only for recipients who are connected now.
        let online_only = matches!(content_body, ContentBody::TypingMessage(_));

        self.restore_thread_timer(&thread, &mut content_body).await;

        let sender_certificate = self.sender_certificate().await?;
        let unidentified_access = self
            .store
            .profile_key(&recipient)
            .await?
            .map(|profile_key| UnidentifiedAccess {
                key: profile_key.derive_access_key().to_vec(),
                certificate: sender_certificate.clone(),
            });

        // we need to put our profile key in DataMessage
        if let ContentBody::DataMessage(message) = &mut content_body {
            message
                .profile_key
                .get_or_insert(self.state.data.profile_key().get_bytes().to_vec());
            message.required_protocol_version = Some(0);
        }

        ensure_data_message_timestamp(&mut content_body, timestamp);

        sender
            .send_message(
                &recipient,
                unidentified_access.as_ref(),
                content_body.clone(),
                timestamp,
                include_pni_signature,
                online_only,
            )
            .await?;

        // save the message
        let content = Content {
            metadata: Metadata {
                sender: self.state.data.service_ids.aci().into(),
                sender_device: self.state.device_id(),
                destination: recipient,
                server_guid: None,
                client_timestamp: chrono::Utc.timestamp_millis_opt(timestamp as i64).unwrap(),
                // Note: Currently no way to get the timestamp the server received the message; just use our timestamp as a fallback.
                server_timestamp: chrono::Utc.timestamp_millis_opt(timestamp as i64).unwrap(),
                needs_receipt: false,
                unidentified_sender: false,
                was_plaintext: false,
                pni_verified: None,
            },
            body: content_body,
        };

        let mut identified_websocket = self.identified_websocket(false).await?;
        save_message(
            &mut self.store,
            &mut identified_websocket,
            content,
            Some(thread),
        )
        .await?;

        Ok(())
    }

    /// Uploads one attachment prior to linking them in a message.
    pub async fn upload_attachment(
        &self,
        spec: AttachmentSpec,
        contents: Vec<u8>,
    ) -> Result<Result<AttachmentPointer, AttachmentUploadError>, Error<S::Error>> {
        Ok(self
            .new_message_sender()
            .await?
            .upload_attachment(spec, contents, &mut rng())
            .await)
    }

    /// Uploads attachments prior to linking them in a message.
    pub async fn upload_attachments(
        &self,
        attachments: Vec<(AttachmentSpec, Vec<u8>)>,
    ) -> Result<Vec<Result<AttachmentPointer, AttachmentUploadError>>, Error<S::Error>> {
        if attachments.is_empty() {
            return Ok(Vec::new());
        }
        let sender = self.new_message_sender().await?;
        let upload = future::join_all(attachments.into_iter().map(move |(spec, contents)| {
            let mut sender = sender.clone();
            async move { sender.upload_attachment(spec, contents, &mut rng()).await }
        }));
        Ok(upload.await)
    }

    /// Sends one message in a group (v2). The `master_key_bytes` is required to have 32 elements.
    ///
    /// This method will automatically update the [DataMessage::expire_timer] if it is set to
    /// [None] such that the chat will keep the current expire timer.
    pub async fn send_message_to_group(
        &mut self,
        master_key_bytes: &[u8],
        message: impl Into<ContentBody>,
        timestamp: u64,
    ) -> Result<(), Error<S::Error>> {
        let mut content_body = message.into();
        let master_key_bytes = master_key_bytes
            .try_into()
            .expect("Master key bytes to be of size 32.");
        let thread = Thread::Group(master_key_bytes);

        self.restore_thread_timer(&thread, &mut content_body).await;
        ensure_data_message_timestamp(&mut content_body, timestamp);

        let mut sender = self.new_message_sender().await?;

        let mut groups_manager = Box::pin(self.groups_manager()).await?;
        let Some(group) =
            upsert_group(&self.store, &mut groups_manager, &master_key_bytes, &0).await?
        else {
            return Err(Error::UnknownGroup);
        };

        let sender_certificate = self.sender_certificate().await?;
        let mut recipients = Vec::new();
        for member in group
            .members
            .into_iter()
            .filter(|m| m.aci != self.state.data.service_ids.aci())
        {
            let unidentified_access =
                self.store
                    .profile_key(&member.aci.into())
                    .await?
                    .map(|profile_key| UnidentifiedAccess {
                        key: profile_key.derive_access_key().to_vec(),
                        certificate: sender_certificate.clone(),
                    });
            let include_pni_signature = false;
            recipients.push((
                member.aci.into(),
                unidentified_access,
                include_pni_signature,
            ));
        }

        // A typing message is only for recipients who are connected now.
        let online_only = matches!(content_body, ContentBody::TypingMessage(_));
        let results = sender
            .send_message_to_group(recipients, content_body.clone(), timestamp, online_only)
            .await;

        // TODO: Handle the NotFound error in the future by removing all sessions to this UUID and marking it as unregistered, not sending any messages to this contact anymore.
        results
            .into_iter()
            .find(|res| match res {
                Ok(_) => false,
                // Ignore any NotFound errors, those mean that e.g. some contact in a group deleted his account.
                Err(MessageSenderError::NotFound { service_id }) => {
                    debug!(service_id = %service_id.service_id_string(), "recipient not found, skipping sent message result");
                    false
                }
                // return first error if any
                Err(_) => true,
            })
            .transpose()?;

        let content = Content {
            metadata: Metadata {
                sender: self.state.data.service_ids.aci().into(),
                destination: self.state.data.service_ids.aci().into(),
                sender_device: self.state.device_id(),
                server_guid: None,
                client_timestamp: chrono::Utc.timestamp_millis_opt(timestamp as i64).unwrap(),
                // Note: Currently no way to get the timestamp the server received the message; just use our timestamp as a fallback.
                server_timestamp: chrono::Utc.timestamp_millis_opt(timestamp as i64).unwrap(),
                needs_receipt: false, // TODO: this is just wrong
                unidentified_sender: false,
                was_plaintext: false,
                pni_verified: None,
            },
            body: content_body,
        };

        let mut identified_websocket = self.identified_websocket(false).await?;
        save_message(
            &mut self.store,
            &mut identified_websocket,
            content,
            Some(thread),
        )
        .await?;

        Ok(())
    }

    async fn restore_thread_timer(&mut self, thread: &Thread, content_body: &mut ContentBody) {
        let store_expire_timer = self.store.expire_timer(thread).await.unwrap_or_default();

        if let ContentBody::DataMessage(DataMessage {
            expire_timer: ref mut timer,
            expire_timer_version: ref mut version,
            ..
        }) = content_body
        {
            if timer.is_none() {
                *timer = store_expire_timer.and_then(|(t, _)| if t == 0 { None } else { Some(t) });
                *version = Some(store_expire_timer.map(|(_, v)| v).unwrap_or_default());
            } else {
                *version = Some(store_expire_timer.map(|(_, v)| v).unwrap_or_default() + 1);
            }
        }
    }

    /// Clears all sessions established with [recipient](ServiceId).
    pub async fn clear_sessions(&self, recipient: &ServiceId) -> Result<(), Error<S::Error>> {
        use libsignal_service::session_store::SessionStoreExt;
        self.store
            .aci_protocol_store()
            .delete_all_sessions(recipient)
            .await?;
        self.store
            .pni_protocol_store()
            .delete_all_sessions(recipient)
            .await?;
        Ok(())
    }

    /// Downloads and decrypts a single attachment.
    pub async fn get_attachment(
        &self,
        attachment_pointer: &AttachmentPointer,
    ) -> Result<Vec<u8>, Error<S::Error>> {
        let expected_digest = attachment_pointer
            .digest
            .as_ref()
            .ok_or_else(|| Error::UnexpectedAttachmentChecksum)?;

        let mut service = self.identified_push_service();
        let mut attachment_stream = service.get_attachment(attachment_pointer).await?;

        let plaintext_len = attachment_pointer.size.and_then(|len| len.try_into().ok());

        // We need the whole file for the crypto to check out
        let mut ciphertext = Vec::with_capacity(plaintext_len.unwrap_or(0));
        let size_bytes = attachment_stream
            .stream
            .read_to_end(&mut ciphertext)
            .await?;
        trace!(size_bytes, "downloaded encrypted attachment");

        let digest = sha2::Sha256::digest(&ciphertext);
        if &digest[..] != expected_digest {
            return Err(Error::UnexpectedAttachmentChecksum);
        }

        let key: [u8; 64] = attachment_pointer.key().try_into()?;

        // Offload decryption of large attachments to another thread.
        // Chose arbitrary threshold here.
        const DECRYPT_IN_THREAD_THRESHOLD: usize = 100 * 1024;
        if ciphertext.len() > DECRYPT_IN_THREAD_THRESHOLD {
            ciphertext = tokio::task::spawn_blocking(move || {
                decrypt_in_place(key, &mut ciphertext).map(|_| ciphertext)
            })
            .await
            .expect("decryption in another thread")?;
        } else {
            decrypt_in_place(key, &mut ciphertext)?;
        };

        if let Some(len) = plaintext_len {
            if len < ciphertext.len() {
                // remove padding
                ciphertext.truncate(len);
            }
        }

        Ok(ciphertext)
    }

    /// Gets the metadata of a sticker
    pub async fn sticker_metadata(
        &mut self,
        pack_id: &[u8],
        sticker_id: u32,
    ) -> Result<Option<Sticker>, Error<S::Error>> {
        Ok(self.store.sticker_pack(pack_id).await?.and_then(|pack| {
            pack.manifest
                .stickers
                .iter()
                .find(|&x| x.id == sticker_id)
                .cloned()
        }))
    }

    /// Installs a sticker pack and notifies other registered devices
    pub async fn install_sticker_pack(
        &mut self,
        pack_id: &[u8],
        pack_key: &[u8],
    ) -> Result<(), Error<S::Error>> {
        let sticker_pack_operation = StickerPackOperation {
            pack_id: Some(pack_id.to_vec()),
            pack_key: Some(pack_key.to_vec()),
            r#type: Some(sticker_pack_operation::Type::Install as i32),
        };

        let unidentified_websocket = self.unidentified_websocket().await?;
        download_sticker_pack(
            self.store.clone(),
            unidentified_websocket,
            &sticker_pack_operation,
        )
        .await?;

        // Sync the change with the other devices
        let sync_message = SyncMessage {
            sticker_pack_operation: vec![sticker_pack_operation],
            ..Default::default()
        };

        let timestamp = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64;

        self.send_message(self.state.data.service_ids.aci(), sync_message, timestamp)
            .await?;

        Ok(())
    }

    /// Removes an installed sticker pack
    pub async fn remove_sticker_pack(
        &mut self,
        pack_id: &[u8],
        pack_key: &[u8],
    ) -> Result<(), Error<S::Error>> {
        // Sync the change with the other clients
        let sync_message = SyncMessage {
            sticker_pack_operation: vec![StickerPackOperation {
                pack_id: Some(pack_id.to_vec()),
                pack_key: Some(pack_key.to_vec()), // The pack key might not be neccesary in the message
                r#type: Some(sticker_pack_operation::Type::Remove as i32),
            }],
            ..Default::default()
        };

        let timestamp = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64;

        self.send_message(self.state.data.service_ids.aci(), sync_message, timestamp)
            .await?;

        self.store.remove_sticker_pack(pack_id).await?;

        Ok(())
    }

    /// Asks a person to send again the message they sent at `timestamp` from `device`, which
    /// could not be decrypted here. It goes through the session with them, so that session has to
    /// work: after a failure in it, start a new one first.
    pub async fn send_retry_request(
        &mut self,
        recipient: &ServiceId,
        timestamp: u64,
        device: DeviceId,
    ) -> Result<(), Error<S::Error>> {
        let mut sender = self.new_message_sender().await?;
        sender
            .send_sender_key_decryption_error_message(recipient, None, timestamp, device)
            .await?;
        Ok(())
    }

    pub async fn send_session_reset(
        &mut self,
        recipient: &ServiceId,
        timestamp: u64,
    ) -> Result<(), Error<S::Error>> {
        trace!(recipient = %recipient.service_id_string(), "resetting session for address");

        let mut store = self.store.aci_protocol_store();
        // The sessions as they are, to put back if the new one cannot be started: a failed send can
        // remove them, and without them the recipient's messages could no longer be decrypted.
        let mut saved = Vec::new();

        // Archive all sessions with all receiver devices.
        // Note that the "get_sub_device_sessions" does not include the main device, therefore add it.
        // Note that deleting the session is not equivalent to archiving:
        // If we deleted the session, we would also delete the information on the list of devices the peer has, failing message sending.
        for device in store
            .get_sub_device_sessions(recipient)
            .await?
            .into_iter()
            .chain(vec![*DEFAULT_DEVICE_ID])
        {
            let address = recipient
                .aci()
                .expect("Recipient to be given as ACI")
                .to_protocol_address(device)
                .expect("Could not construct protocol address from recipient");
            if let Some(mut session) = store.load_session(&address).await? {
                saved.push((address.clone(), session.clone()));
                session.archive_current_state()?;
                store.store_session(&address, &session).await?;
            }
        }

        // TODO: Signal Android also deletes entries in some sender_key_shared table; we don't have such a table yet.
        // Does not seem necessary though.

        // Send a null message.
        let message = NullMessage::generate(&mut rand::rng());
        if let Err(error) = self.send_message(*recipient, message, timestamp).await {
            for (address, session) in &saved {
                store.store_session(address, session).await?;
            }
            return Err(error);
        }

        Ok(())
    }

    fn credentials(&self) -> ServiceCredentials {
        self.state.credentials()
    }

    /// Creates a new message sender.
    async fn new_message_sender(&self) -> Result<MessageSender<S::AciStore>, Error<S::Error>> {
        let identified_websocket = self.identified_websocket(false).await?;
        let unidentified_websocket = self.unidentified_websocket().await?;

        let aci_protocol_store = self.store.aci_protocol_store();
        let aci_identity_keypair = aci_protocol_store.get_identity_key_pair().await?;
        let pni_identity_keypair = self
            .store
            .pni_protocol_store()
            .get_identity_key_pair()
            .await?;

        Ok(MessageSender::new(
            identified_websocket,
            unidentified_websocket,
            self.identified_push_service(),
            self.new_service_cipher_aci(),
            aci_protocol_store,
            self.state.data.service_ids.aci,
            self.state.data.service_ids.pni,
            aci_identity_keypair,
            Some(pni_identity_keypair),
            self.state.device_id(),
        ))
    }

    fn new_service_cipher_aci(&self) -> ServiceCipher<S::AciStore> {
        ServiceCipher::new(
            self.store.aci_protocol_store(),
            self.state
                .service_configuration()
                .unidentified_sender_trust_roots,
            ProtocolAddress::new(
                self.state.data.service_ids.aci().service_id_string(),
                self.state.device_id(),
            ),
        )
    }

    fn new_service_cipher_pni(&self) -> ServiceCipher<S::PniStore> {
        ServiceCipher::new(
            self.store.pni_protocol_store(),
            self.state
                .service_configuration()
                .unidentified_sender_trust_roots,
            ProtocolAddress::new(
                self.state.data.service_ids.pni().service_id_string(),
                self.state.device_id(),
            ),
        )
    }

    /// Returns the title of a thread (contact or group).
    pub async fn thread_title(&self, thread: &Thread) -> Result<String, Error<S::Error>> {
        match thread {
            Thread::Contact(service_id) => {
                let contact = match self.store.contact_by_id(service_id).await {
                    Ok(contact) => contact,
                    Err(error) => {
                        info!(%error, service_id =% service_id.service_id_string(), "error getting contact by id");
                        None
                    }
                };
                Ok(match contact {
                    Some(contact) => contact.name,
                    None => service_id.service_id_string(),
                })
            }
            Thread::Group(id) => match self.store.group(*id).await? {
                Some(group) => Ok(group.title),
                None => Ok("".to_string()),
            },
        }
    }

    /// Returns how this client was registered, either as a primary or secondary device.
    pub fn registration_type(&self) -> RegistrationType {
        if self.state.data.device_name.is_some() {
            RegistrationType::Secondary
        } else {
            RegistrationType::Primary
        }
    }

    /// As a primary device, link a secondary device.
    pub async fn link_secondary(&mut self, secondary: Url) -> Result<(), Error<S::Error>> {
        // XXX: What happens if secondary device? Possible to use static typing to make this method call impossible in that case?
        if self.registration_type() != RegistrationType::Primary {
            return Err(Error::NotPrimaryDevice);
        }

        let credentials = self.credentials();
        let mut account_manager = AccountManager::new(
            self.identified_push_service(),
            self.identified_websocket(false).await?,
            Some(self.state.data.profile_key),
        );

        account_manager
            .link_device(
                &mut rand::rng(),
                secondary,
                &self.store.aci_protocol_store(),
                &self.store.pni_protocol_store(),
                ProvisioningSecrets {
                    credentials,
                    account_entropy_pool: self
                        .account_entropy_pool()
                        .await?
                        .expect("Primary device to always have an account entropy pool"),
                    ephemeral_backup_key: None,
                    media_root_backup_key: None,
                },
            )
            .await?;
        Ok(())
    }

    /// As a primary device, unlink a secondary device.
    pub async fn unlink_secondary(
        &self,
        device_id: impl TryInto<DeviceId>,
    ) -> Result<(), Error<S::Error>> {
        // secondary devices cannot unlink themselves or other devices, it will fail with an unauthorized error
        if self.registration_type() != RegistrationType::Primary {
            return Err(Error::NotPrimaryDevice);
        }
        self.identified_websocket(false)
            .await?
            .unlink_device(device_id.try_into().map_err(|_| Error::InvalidDeviceId)?)
            .await?;
        Ok(())
    }

    /// As a primary device, list all the devices (including the current device).
    pub async fn devices(&self) -> Result<Vec<DeviceInfo>, Error<S::Error>> {
        let aci_protocol_store = self.store.aci_protocol_store();
        let mut account_manager = AccountManager::new(
            self.identified_push_service(),
            self.identified_websocket(false).await?,
            Some(self.state.data.profile_key),
        );

        Ok(account_manager.linked_devices(&aci_protocol_store).await?)
    }
}

/// Set the timestamp in any DataMessage so it matches its envelope's
fn ensure_data_message_timestamp(content_body: &mut ContentBody, timestamp: u64) {
    match content_body {
        ContentBody::DataMessage(message) => {
            message.timestamp = Some(timestamp);
        }
        ContentBody::EditMessage(EditMessage {
            data_message: Some(data_message),
            ..
        }) => {
            data_message.timestamp = Some(timestamp);
        }
        ContentBody::SynchronizeMessage(SyncMessage {
            content:
                Some(SyncContent::Sent(sync_message::Sent {
                    message: Some(data_message),
                    ..
                })),
            ..
        }) => {
            data_message.timestamp = Some(timestamp);
        }
        _ => (),
    }
}

async fn upsert_group<S: Store>(
    store: &S,
    groups_manager: &mut GroupsManager<InMemoryCredentialsCache>,
    master_key_bytes: &[u8],
    revision: &u32,
) -> Result<Option<Group>, Error<S::Error>> {
    let upsert_group = match store.group(master_key_bytes.try_into()?).await {
        Ok(Some(group)) => {
            debug!(group_name =% group.title, "loaded group from local db");
            group.revision < *revision
        }
        Ok(None) => true,
        Err(error) => {
            warn!(%error, "failed to retrieve group from local db");
            true
        }
    };

    if upsert_group {
        debug!("fetching and saving group");
        match groups_manager
            .fetch_encrypted_group(&mut rand::rng(), master_key_bytes)
            .await
        {
            Ok(encrypted_group) => {
                let group = decrypt_group(master_key_bytes, encrypted_group)?;
                if let Err(error) = store.save_group(master_key_bytes.try_into()?, group).await {
                    error!(%error, "failed to save group");
                }
            }
            Err(error) => {
                warn!(%error, "failed to fetch encrypted group")
            }
        }
    }

    Ok(store.group(master_key_bytes.try_into()?).await?)
}

/// Download and decrypt a sticker manifest
async fn download_sticker_pack<C: ContentsStore>(
    mut store: C,
    mut unidentified_websocket: SignalWebSocket<websocket::Unidentified>,
    operation: &StickerPackOperation,
) -> Result<StickerPack, Error<C::ContentsStoreError>> {
    debug!("downloading sticker pack");
    let pack_key = operation.pack_key();
    let pack_id = operation.pack_id();
    let key = derive_key(pack_key)?;

    let mut ciphertext = Vec::new();

    let size_bytes = unidentified_websocket
        .get_sticker_pack_manifest(&hex::encode(pack_id))
        .await?
        .read_to_end(&mut ciphertext)
        .await?;

    trace!(size_bytes, "downloaded encrypted sticker pack manifest");

    decrypt_in_place(key, &mut ciphertext)?;

    let mut sticker_pack_manifest: StickerPackManifest =
        libsignal_service::proto::Pack::decode(ciphertext.as_slice())
            .map_err(ProvisioningError::from)?
            .into();

    for sticker in &mut sticker_pack_manifest.stickers {
        match download_sticker::<C>(&mut unidentified_websocket, pack_id, pack_key, sticker.id)
            .await
        {
            Ok(decrypted_sticker_bytes) => {
                debug!(id = sticker.id, "downloaded sticker");
                sticker.bytes = Some(decrypted_sticker_bytes);
            }
            Err(error) => error!(sticker.id, %error,"failed to download sticker"),
        }
    }

    let sticker_pack = StickerPack {
        id: pack_id.to_vec(),
        key: pack_key.to_vec(),
        manifest: sticker_pack_manifest,
    };

    // save everything in store
    store.add_sticker_pack(&sticker_pack).await?;

    Ok(sticker_pack)
}

/// Downloads and decrypts a single sticker
async fn download_sticker<C: ContentsStore>(
    unidentified_websocket: &mut SignalWebSocket<websocket::Unidentified>,
    pack_id: &[u8],
    pack_key: &[u8],
    sticker_id: u32,
) -> Result<Vec<u8>, Error<C::ContentsStoreError>> {
    let key = derive_key(pack_key)?;

    let mut sticker_stream = unidentified_websocket
        .get_sticker(&hex::encode(pack_id), sticker_id)
        .await?;

    let mut ciphertext = Vec::new();
    let size_bytes = sticker_stream.read_to_end(&mut ciphertext).await?;

    trace!(size_bytes, "downloaded encrypted sticker");

    decrypt_in_place(key, &mut ciphertext)?;

    Ok(ciphertext)
}

/// Save a message into the store.
/// Note that `override_thread` can be used to specify the thread the message will be stored in.
/// This is required when storing outgoing messages, as in this case the appropriate storage place cannot be derived from the message itself.
async fn save_message<S: Store>(
    store: &mut S,
    identified_websocket: &mut websocket::SignalWebSocket<websocket::Identified>,
    message: Content,
    override_thread: Option<Thread>,
) -> Result<(), Error<S::Error>> {
    // derive the thread from the message type
    let thread = override_thread.unwrap_or(Thread::try_from(&message)?);

    // only save DataMessage and SynchronizeMessage (sent)
    let message = match message.body {
        ContentBody::DecryptionErrorMessage(e) => {
            warn!(error = ?e, "was asked to save a DecryptionErrorMessage; this should not happen");
            None
        }
        ContentBody::NullMessage(_) => Some(message),
        ContentBody::DataMessage(
            ref data_message @ DataMessage {
                ref profile_key, ..
            },
        )
        | ContentBody::SynchronizeMessage(SyncMessage {
            content:
                Some(SyncContent::Sent(sync_message::Sent {
                    message:
                        Some(
                            ref data_message @ DataMessage {
                                ref profile_key, ..
                            },
                        ),
                    ..
                })),
            ..
        }) => {
            // update recipient profile key if changed
            if let Some(profile_key_bytes) = profile_key.clone().and_then(|p| p.try_into().ok()) {
                let sender = message.metadata.sender;
                let profile_key = ProfileKey::create(profile_key_bytes);
                debug!(sender = %sender.service_id_string(), "inserting profile key for");

                // Either:
                // - insert a new contact with the profile information
                // - update the contact if the profile key has changed
                // TODO: mark this contact as "created by us" maybe to know whether we should update it or not
                // NOTE: this needs to happen in the background!
                let store_inner = store.clone();
                let websocket_inner = identified_websocket.clone();
                let data_message_inner = data_message.clone();
                tokio::spawn(async move {
                    if let Err(error) = upsert_contact_from_profile(
                        store_inner,
                        websocket_inner,
                        &data_message_inner,
                        sender,
                        profile_key,
                    )
                    .await
                    {
                        error!(%error, "failed to upsert newly seen contact!");
                    }
                });
            }

            // Note: The expire timer fields of data messages are only for contacts.
            // Expire timers are handled for groups via upsert_group due to a revision change.
            if let Thread::Contact(_) = thread {
                let version = data_message.expire_timer_version.unwrap_or(1);
                store
                    .update_expire_timer(
                        &thread,
                        data_message.expire_timer.unwrap_or_default(),
                        version,
                    )
                    .await?;
            }

            match data_message {
                DataMessage {
                    delete:
                        Some(Delete {
                            target_sent_timestamp: Some(ts),
                        }),
                    ..
                } => {
                    // replace an existing message by an empty NullMessage
                    if let Some(mut existing_msg) = store.message(&thread, *ts).await? {
                        existing_msg.metadata.sender = Aci::from(Uuid::nil()).into();
                        existing_msg.body = NullMessage::default().into();
                        store.save_message(&thread, existing_msg).await?;
                        debug!(%thread, ts, "message in thread deleted");
                        None
                    } else {
                        warn!(%thread, ts, "could not find message to delete in thread");
                        None
                    }
                }
                _ => Some(message),
            }
        }
        ContentBody::SynchronizeMessage(SyncMessage {
            content: Some(SyncContent::DeleteForMe(ref delete)),
            ..
        }) => {
            // TODO: Conversations, local-only deletes, attachments
            for d in delete.message_deletes.iter().flat_map(|m| &m.messages) {
                let sender = match &d.author {
                    Some(Author::AuthorServiceId(id)) => {
                        ServiceId::parse_from_service_id_string(&id)
                    }
                    Some(Author::AuthorServiceIdBinary(id)) => {
                        ServiceId::parse_from_service_id_binary(&id)
                    }
                    Some(Author::AuthorE164(_)) => None,
                    None => None,
                };
                let Some(sender) = sender else {
                    tracing::warn!("Could not parse author of delete-for-self message; ignoring.");
                    continue;
                };
                let Some(timestamp) = d.sent_timestamp else {
                    tracing::warn!("Timestamp of delete-for-self message not given; ignoring.");
                    continue;
                };
                let Ok(Some(thread)) = store
                    .thread_for_sender_and_timestamp(&sender, timestamp)
                    .await
                else {
                    tracing::warn!(
                        "Message referenced by delete-for-self message not found; ignoring."
                    );
                    continue;
                };
                // Note: Not marking the message as deleted, like when receiving deletion requests by others.
                // This matches the behavior of Signal Desktop, where the message completely disappears from the timeline.
                let result = store.delete_message(&thread, timestamp).await;
                if !result.is_ok_and(|d| d) {
                    tracing::warn!(
                        "Could not delete message referenced by delete-for-self message; ignoring."
                    );
                }
            }
            None
        }
        ContentBody::EditMessage(EditMessage {
            target_sent_timestamp: Some(_),
            data_message: Some(_),
        })
        | ContentBody::SynchronizeMessage(SyncMessage {
            content:
                Some(SyncContent::Sent(sync_message::Sent {
                    edit_message:
                        Some(EditMessage {
                            target_sent_timestamp: Some(_),
                            data_message: Some(_),
                        }),
                    ..
                })),
            ..
        }) => Some(message),
        ContentBody::CallMessage(_)
        | ContentBody::SynchronizeMessage(SyncMessage {
            content: Some(SyncContent::CallEvent(_)),
            ..
        }) => Some(message),
        ContentBody::SynchronizeMessage(msg) => {
            debug!(
                ?msg,
                "skipping saving sync message without interesting fields"
            );
            None
        }
        ContentBody::ReceiptMessage(_) => Some(message),
        ContentBody::TypingMessage(msg) => {
            debug!(?msg, "skipping saving typing message");
            None
        }
        ContentBody::StoryMessage(msg) => {
            debug!(?msg, "skipping story message");
            None
        }
        ContentBody::EditMessage(msg) => {
            debug!(?msg, "invalid edited");
            None
        }
    };

    if let Some(message) = message {
        store.save_message(&thread, message).await?;
    }

    Ok(())
}

async fn upsert_contact_from_profile<S: Store>(
    mut store: S,
    mut identified_websocket: SignalWebSocket<websocket::Identified>,
    data_message: &DataMessage,
    sender: ServiceId,
    profile_key: ProfileKey,
) -> Result<(), Error<<S as Store>::Error>> {
    if store.contact_by_id(&sender).await?.is_none()
        || store
            .profile_key(&sender)
            .await?
            .is_none_or(|p| p.bytes != profile_key.bytes)
    {
        if let Some(aci) = sender.aci() {
            let sender_uuid: Uuid = aci.into();
            let encrypted_profile = identified_websocket
                .retrieve_profile_by_id(aci, Some(profile_key))
                .await?;
            let profile_cipher = ProfileCipher::new(profile_key);
            // (the key is the one the message carried, which need not be the one the sender's
            // profile is kept under: then the profile is not read, and the program goes on)
            let decrypted_profile = profile_cipher.decrypt(encrypted_profile)?;

            let contact = Contact {
                uuid: sender_uuid,
                phone_number: None,
                name: decrypted_profile
                    .name
                    // FIXME: this assumes [firstname] [lastname]
                    .map(|pn| {
                        if let Some(family_name) = pn.family_name {
                            format!("{} {}", pn.given_name, family_name)
                        } else {
                            pn.given_name
                        }
                    })
                    .unwrap_or_default(),
                profile_key: profile_key.bytes.to_vec(),
                expire_timer: data_message.expire_timer.unwrap_or_default(),
                expire_timer_version: data_message.expire_timer_version.unwrap_or(1),
                inbox_position: 0,
                avatar: None,
                verified: Verified::default(),
            };

            info!(%sender_uuid, "saved contact on first sight");
            store.save_contact(&contact).await?;
            store.upsert_profile_key(&sender_uuid, profile_key).await?;
        } else {
            debug!("not storing profile for PNI contact");
        }
    }
    Ok(())
}

async fn set_account_attributes<S: Store>(
    account_manager: &mut AccountManager,
    store: &S,
    data: &RegistrationData,
) -> Result<(), Error<S::Error>> {
    trace!("setting account attributes");

    let pni_registration_id = data.pni_registration_id.ok_or(Error::RelinkNecessary)?;

    let name = if let Some(device_name) = data.device_name() {
        let aci_key_pair = store.aci_protocol_store().get_identity_key_pair().await?;
        let mut rng = rng();
        Some(encrypt_device_name(
            &mut rng,
            device_name,
            aci_key_pair.identity_key(),
        )?)
    } else {
        None
    };

    account_manager
        .set_account_attributes(AccountAttributes {
            fetches_messages: true,
            registration_id: data.registration_id,
            pni_registration_id,
            name,
            registration_lock: None,
            unidentified_access_key: Some(data.profile_key.derive_access_key().to_vec()),
            unrestricted_unidentified_access: false,
            capabilities: Some(DeviceCapabilities {
                storage: true,
                transfer: false,
                attachment_backfill: false,
                spqr: true,
                profiles_v2: false,
                username_change_sync_message: true,
            }),
            discoverable_by_phone_number: true,
            recovery_password: None,
            video: false,
            voice: false,
        })
        .await?;

    trace!("done setting account attributes");
    Ok(())
}

async fn register_pre_keys<S: Store>(
    store: &S,
    account_manager: &mut AccountManager,
) -> Result<(), Error<S::Error>> {
    trace!("registering pre keys");

    account_manager
        .update_pre_key_bundle(&mut store.aci_protocol_store(), ServiceIdKind::Aci, true)
        .await?;

    account_manager
        .update_pre_key_bundle(&mut store.pni_protocol_store(), ServiceIdKind::Pni, true)
        .await?;

    trace!("registered pre keys");
    Ok(())
}

/// Downloads and reads the contacts the primary device sent: records of ContactDetails, each
/// preceded by its length and followed by its picture when it has one. Unlike libsignal-service's
/// own reader, which panics on it, a record or picture that runs past the end of the data ends the
/// list, keeping what came before: a phone's sync has been seen claiming a picture of 4345 bytes
/// with 3177 left.
async fn retrieve_contacts(
    service: &mut PushService,
    contacts: &sync_message::Contacts,
) -> Result<Vec<libsignal_service::models::Contact>, libsignal_service::push_service::ServiceError> {
    use libsignal_service::push_service::ServiceError;

    let Some(blob) = &contacts.blob else { return Ok(Vec::new()) };
    let mut ciphertext = Vec::new();
    service.get_attachment(blob).await?.stream.read_to_end(&mut ciphertext).await?;
    let key: [u8; 64] = blob.key().try_into().map_err(|_| ServiceError::InvalidFrame { reason: "contacts key is not 64 bytes" })?;
    decrypt_in_place(key, &mut ciphertext).map_err(|_| ServiceError::InvalidFrame { reason: "contacts did not decrypt" })?;
    // (the attachment is padded past the size it gives)
    if let Some(size) = blob.size.and_then(|size| usize::try_from(size).ok()) {
        ciphertext.truncate(size);
    }
    Ok(read_contacts(ciphertext))
}

/// The contacts in a decrypted contacts attachment, as far as they read.
fn read_contacts(plaintext: Vec<u8>) -> Vec<libsignal_service::models::Contact> {
    use bytes::Buf;
    use libsignal_service::proto::ContactDetails;

    let total = plaintext.len();
    let mut data = bytes::Bytes::from(plaintext);
    let mut read = Vec::new();
    while data.has_remaining() {
        let details = match ContactDetails::decode_length_delimited(&mut data) {
            Ok(details) => details,
            Err(error) => {
                warn!(%error, offset = total - data.remaining(), total, "contacts: a record does not read; the list ends there");
                break;
            },
        };
        let length = details.avatar.as_ref().and_then(|avatar| avatar.length).unwrap_or(0) as usize;
        if length > data.remaining() {
            warn!(length, left = data.remaining(), total, "contacts: a picture runs past the end; the list ends there");
            if let Ok(contact) = libsignal_service::models::Contact::from_proto(details, None) {
                read.push(contact);
            }
            break;
        }
        let picture = (length > 0).then(|| data.copy_to_bytes(length));
        match libsignal_service::models::Contact::from_proto(details, picture) {
            Ok(contact) => read.push(contact),
            Err(error) => debug!(%error, "contacts: a record is skipped"),
        }
    }
    read
}

#[cfg(test)]
mod contacts_tests {
    use libsignal_service::prelude::ProtobufMessage;
    use libsignal_service::proto::{contact_details::Avatar, ContactDetails};

    fn record(aci: &str, name: &str, picture: Option<(u32, &[u8])>) -> Vec<u8> {
        let details = ContactDetails {
            aci: Some(aci.into()),
            name: Some(name.into()),
            avatar: picture.map(|(length, _)| Avatar { content_type: Some("image/jpeg".into()), length: Some(length) }),
            ..Default::default()
        };
        let mut out = details.encode_length_delimited_to_vec();
        if let Some((_, bytes)) = picture {
            out.extend_from_slice(bytes);
        }
        out
    }

    const A: &str = "11111111-1111-4111-8111-111111111111";
    const B: &str = "22222222-2222-4222-8222-222222222222";

    #[test]
    fn reads_records_with_and_without_pictures() {
        let mut data = record(A, "Ann", Some((3, b"abc")));
        data.extend(record(B, "Bob", None));
        let read = super::read_contacts(data);
        assert_eq!(read.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Ann", "Bob"]);
        assert!(read[0].avatar.is_some() && read[1].avatar.is_none());
    }

    #[test]
    fn a_picture_past_the_end_keeps_what_came_before() {
        let mut data = record(A, "Ann", None);
        data.extend(record(B, "Bob", Some((4345, &[0u8; 3177]))));
        let read = super::read_contacts(data);
        assert_eq!(read.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Ann", "Bob"]);
        assert!(read[1].avatar.is_none());
    }
}
