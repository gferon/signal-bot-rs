use std::{convert::TryInto, time::UNIX_EPOCH};

use futures::{
    channel::mpsc::{channel, Sender},
    future, pin_mut, SinkExt, Stream, StreamExt,
};
use image::Luma;
use log::{error, trace, warn};
use qrcode::QrCode;
use rand::{distributions::Alphanumeric, CryptoRng, Rng, RngCore};

use libsignal_service::{
    cipher::ServiceCipher,
    configuration::{ServiceConfiguration, SignalServers, SignalingKey},
    content::{ContentBody, DataMessage, Metadata},
    groups_v2::{GroupsManager, InMemoryCredentialsCache},
    messagepipe::ServiceCredentials,
    models::Contact,
    prelude::{
        phonenumber::PhoneNumber,
        protocol::{
            IdentityKeyStore, KeyPair, PreKeyStore, PrivateKey, PublicKey, SessionStore,
            SignedPreKeyStore,
        },
        Content, Envelope, GroupMasterKey, GroupSecretParams, MessageSender, PushService, Uuid,
    },
    proto::{sync_message, SyncMessage},
    provisioning::{
        generate_registration_id, ConfirmCodeMessage, LinkingManager, ProvisioningManager,
        SecondaryDeviceProvisioning, VerificationCodeResponse,
    },
    push_service::{ProfileKey, ServiceError, WhoAmIResponse, DEFAULT_DEVICE_ID},
    receiver::MessageReceiver,
    session_store::SessionStoreExt,
    AccountManager, Profile, ServiceAddress,
};

use libsignal_service_hyper::push_service::HyperPushService;

use crate::{config::ConfigStore, Error};

#[derive(Clone)]
pub struct Manager<C, I, S, SP, P, R> {
    config_store: C,
    identity_key_store: I,
    session_store: S,
    signed_pre_key_store: SP,
    pre_key_store: P,
    state: State,
    csprng: R,
}

#[derive(Clone)]
pub enum State {
    New,
    Registration {
        signal_servers: SignalServers,
        phone_number: PhoneNumber,
        password: String,
    },
    Registered {
        signal_servers: SignalServers,
        phone_number: PhoneNumber,
        uuid: Uuid,
        password: String,
        signaling_key: SignalingKey,
        device_id: Option<u32>,
        registration_id: u32,
        private_key: PrivateKey,
        public_key: PublicKey,
        profile_key: [u8; 32],
    },
}

impl<C, I, S, SP, P, R> Manager<C, I, S, SP, P, R>
where
    C: ConfigStore,
    S: SessionStore + SessionStoreExt + Clone,
    I: IdentityKeyStore + Clone,
    SP: SignedPreKeyStore + Clone,
    P: PreKeyStore + Clone,
    R: Rng + CryptoRng + Clone,
{
    pub fn new(
        config_store: C,
        session_store: S,
        identity_key_store: I,
        signed_pre_key_store: SP,
        pre_key_store: P,
        csprng: R,
    ) -> Result<Self, Error> {
        let state = config_store.state()?;
        Ok(Manager {
            config_store,
            identity_key_store,
            session_store,
            signed_pre_key_store,
            pre_key_store,
            csprng,
            state,
        })
    }

    fn save(&self) -> Result<(), Error> {
        trace!("saving configuration");
        self.config_store.save(&self.state)
    }

    fn credentials(&self) -> Result<ServiceCredentials, Error> {
        match &self.state {
            State::New => Err(Error::NotYetRegisteredError),
            State::Registration { .. } => Err(Error::NotYetRegisteredError),
            State::Registered {
                phone_number,
                uuid,
                device_id,
                password,
                signaling_key,
                ..
            } => Ok(ServiceCredentials {
                uuid: Some(*uuid),
                phonenumber: phone_number.clone(),
                password: Some(password.clone()),
                signaling_key: Some(*signaling_key),
                device_id: *device_id,
            }),
        }
    }

    /// Checks if the manager has a registered device.
    pub fn is_registered(&self) -> bool {
        match &self.state {
            State::Registered { .. } => true,
            _ => false,
        }
    }

    pub fn config_store(&self) -> &C {
        &self.config_store
    }

    pub fn uuid(&self) -> Uuid {
        match &self.state {
            State::Registered { uuid, .. } => *uuid,
            _ => Default::default(),
        }
    }

    pub fn phone_number(&self) -> Option<&PhoneNumber> {
        match &self.state {
            State::Registered { phone_number, .. } => Some(phone_number),
            _ => None,
        }
    }

    pub async fn register(
        &mut self,
        signal_servers: SignalServers,
        phone_number: PhoneNumber,
        use_voice_call: bool,
        captcha: Option<&str>,
    ) -> Result<(), Error> {
        // generate a random 24 bytes password
        let rng = rand::rngs::OsRng::default();
        let password: String = rng.sample_iter(&Alphanumeric).take(24).collect();

        let cfg: ServiceConfiguration = signal_servers.into();
        let mut provisioning_manager: ProvisioningManager<HyperPushService> =
            ProvisioningManager::new(
                cfg,
                crate::USER_AGENT.to_string(),
                phone_number.clone(),
                password.clone(),
            );

        let verification_code_response = if use_voice_call {
            provisioning_manager
                .request_voice_verification_code(captcha.as_deref(), None)
                .await?
        } else {
            provisioning_manager
                .request_sms_verification_code(captcha.as_deref(), None)
                .await?
        };

        if let VerificationCodeResponse::CaptchaRequired = verification_code_response {
            return Err(Error::CaptchaRequired);
        }

        self.state = State::Registration {
            signal_servers,
            phone_number,
            password,
        };

        self.save()?;
        Ok(())
    }

    pub async fn confirm_verification_code(&mut self, confirm_code: u32) -> Result<(), Error> {
        trace!("confirming verification code");
        let (signal_servers, phone_number, password) = match &self.state {
            State::New => return Err(Error::NotYetRegisteredError),
            State::Registration {
                signal_servers,
                phone_number,
                password,
            } => (signal_servers, phone_number, password),
            State::Registered { .. } => return Err(Error::AlreadyRegisteredError),
        };

        // see libsignal-protocol-c / signal_protocol_key_helper_generate_registration_id
        let registration_id = generate_registration_id(&mut self.csprng);
        trace!("registration_id: {}", registration_id);

        // let mut push_service = HyperPushService::new(
        //     (*signal_servers).into(),
        //     Some(ServiceCredentials {
        //         phonenumber: phone_number.clone(),
        //         password: Some(password.clone()),
        //         uuid: None,
        //         signaling_key: None,
        //         device_id: None,
        //     }),
        //     USER_AGENT,
        // );
        let cfg: ServiceConfiguration = (*signal_servers).into();
        let mut provisioning_manager: ProvisioningManager<HyperPushService> =
            ProvisioningManager::new(
                cfg,
                crate::USER_AGENT.to_string(),
                phone_number.clone(),
                password.to_string(),
            );

        let mut rng = rand::rngs::OsRng::default();
        // generate a 52 bytes signaling key
        let mut signaling_key = [0u8; 52];
        rng.fill_bytes(&mut signaling_key);

        let mut profile_key = [0u8; 32];
        rng.fill_bytes(&mut profile_key);
        let profile_key = ProfileKey(profile_key);

        let registered = provisioning_manager
            .confirm_verification_code(
                confirm_code,
                ConfirmCodeMessage::new(
                    signaling_key.to_vec(),
                    registration_id,
                    profile_key.derive_access_key(),
                ),
            )
            .await?;

        let identity_key_pair = KeyPair::generate(&mut self.csprng);

        self.state = State::Registered {
            signal_servers: *signal_servers,
            phone_number: phone_number.clone(),
            uuid: registered.uuid,
            password: password.clone(),
            signaling_key,
            device_id: None,
            registration_id,
            private_key: identity_key_pair.private_key,
            public_key: identity_key_pair.public_key,
            profile_key: profile_key.0,
        };

        trace!("confirmed! (and registered)");

        self.save()?;

        self.register_pre_keys().await?;

        Ok(())
    }

    pub async fn link_secondary_device(
        &mut self,
        signal_servers: SignalServers,
        device_name: String,
    ) -> Result<(), Error> {
        // generate a random 24 bytes password
        let mut rng = rand::rngs::OsRng::default();
        let password: String = rng.sample_iter(&Alphanumeric).take(24).collect();

        // generate a 52 bytes signaling key
        let mut signaling_key = [0u8; 52];
        rng.fill_bytes(&mut signaling_key);

        let mut linking_manager: LinkingManager<HyperPushService> = LinkingManager::new(
            signal_servers,
            crate::USER_AGENT.to_string(),
            password.clone(),
        );

        let (tx, mut rx) = channel(1);

        let (fut1, fut2) = future::join(
            linking_manager.provision_secondary_device(
                &mut self.csprng,
                signaling_key,
                &device_name,
                tx,
            ),
            async move {
                while let Some(provisioning_step) = rx.next().await {
                    match provisioning_step {
                        SecondaryDeviceProvisioning::Url(url) => {
                            log::info!("generating qrcode from provisioning link: {}", &url);
                            let code =
                                QrCode::new(url.as_str()).expect("failed to generate qrcode");
                            let image = code.render::<Luma<u8>>().build();
                            let path = std::env::temp_dir().join("device-link.png");
                            image.save(&path).map_err(|e| {
                                log::error!("failed to generate qr code: {}", e);
                                Error::QrCodeError
                            })?;
                            opener::open(path).map_err(|e| {
                                log::error!("failed to open qr code: {}", e);
                                Error::QrCodeError
                            })?;
                        }
                        SecondaryDeviceProvisioning::NewDeviceRegistration {
                            phone_number,
                            device_id,
                            registration_id,
                            uuid,
                            private_key,
                            public_key,
                            profile_key,
                        } => {
                            log::info!("successfully registered device {}", &uuid);
                            return Ok((
                                phone_number,
                                device_id.device_id,
                                registration_id,
                                uuid,
                                private_key,
                                public_key,
                                profile_key,
                            ));
                        }
                    }
                }
                Err(Error::NoProvisioningMessageReceived)
            },
        )
        .await;

        let _ = fut1?;
        let (phone_number, device_id, registration_id, uuid, private_key, public_key, profile_key) =
            fut2?;

        self.state = State::Registered {
            signal_servers,
            phone_number,
            uuid,
            signaling_key,
            password,
            device_id: Some(device_id),
            registration_id,
            public_key,
            private_key,
            profile_key: profile_key.try_into().unwrap(),
        };

        self.save()?;
        self.register_pre_keys().await?;
        Ok(())
    }

    pub async fn whoami(&self) -> Result<WhoAmIResponse, Error> {
        // TODO: also factor this
        let signal_servers = match &self.state {
            State::New | State::Registration { .. } => return Err(Error::NotYetRegisteredError),
            State::Registered { signal_servers, .. } => signal_servers,
        };

        let credentials = self.credentials()?;
        let service_configuration: ServiceConfiguration = (*signal_servers).into();

        let mut push_service = HyperPushService::new(
            service_configuration,
            Some(credentials),
            crate::USER_AGENT.to_string(),
        );

        Ok(push_service.whoami().await?)
    }

    pub async fn retrieve_profile(&self) -> Result<Profile, Error> {
        match &self.state {
            State::New | State::Registration { .. } => Err(Error::NotYetRegisteredError),
            State::Registered {
                uuid, profile_key, ..
            } => self.retrieve_profile_by_uuid(*uuid, *profile_key).await,
        }
    }

    pub async fn retrieve_profile_by_uuid(
        &self,
        uuid: Uuid,
        profile_key: [u8; 32],
    ) -> Result<Profile, Error> {
        let signal_servers = match &self.state {
            State::New | State::Registration { .. } => return Err(Error::NotYetRegisteredError),
            State::Registered { signal_servers, .. } => signal_servers,
        };

        let credentials = self.credentials()?;
        let service_configuration: ServiceConfiguration = (*signal_servers).into();

        let push_service = HyperPushService::new(
            service_configuration,
            Some(credentials.clone()),
            crate::USER_AGENT.to_string(),
        );

        let mut account_manager = AccountManager::new(push_service, Some(profile_key));

        Ok(account_manager.retrieve_profile(uuid).await?)
    }

    pub async fn register_pre_keys(&mut self) -> Result<(), Error> {
        let (signal_servers, profile_key) = match &self.state {
            State::New | State::Registration { .. } => return Err(Error::NotYetRegisteredError),
            State::Registered {
                signal_servers,
                profile_key,
                ..
            } => (signal_servers, profile_key),
        };

        let cfg: ServiceConfiguration = (*signal_servers).into();
        let credentials = self.credentials()?;
        let push_service =
            HyperPushService::new(cfg, Some(credentials), crate::USER_AGENT.to_string());

        let mut account_manager = AccountManager::new(push_service, Some(profile_key.clone()));

        let (pre_keys_offset_id, next_signed_pre_key_id) = account_manager
            .update_pre_key_bundle(
                &mut self.config_store.clone(),
                &mut self.config_store.clone(),
                &mut self.config_store.clone(),
                &mut self.csprng,
                self.config_store.pre_keys_offset_id()?,
                self.config_store.next_signed_pre_key_id()?,
                true,
            )
            .await?;

        self.config_store
            .set_pre_keys_offset_id(pre_keys_offset_id)?;
        self.config_store
            .set_next_signed_pre_key_id(next_signed_pre_key_id)?;

        Ok(())
    }

    pub async fn request_contacts_sync(&self) -> Result<(), Error> {
        let phone_number = match &self.state {
            State::New | State::Registration { .. } => return Err(Error::NotYetRegisteredError),
            State::Registered { phone_number, .. } => phone_number,
        };

        let sync_message = SyncMessage {
            request: Some(sync_message::Request {
                r#type: Some(sync_message::request::Type::Contacts as i32),
            }),
            ..Default::default()
        };

        let timestamp = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64;

        self.send_message(phone_number.clone(), sync_message, timestamp)
            .await?;

        Ok(())
    }

    async fn receive_messages_encrypted_stream(
        &self,
    ) -> Result<impl Stream<Item = Result<Envelope, ServiceError>>, Error> {
        let signal_servers = match &self.state {
            State::New | State::Registration { .. } => return Err(Error::NotYetRegisteredError),
            State::Registered { signal_servers, .. } => signal_servers,
        };

        // TODO: error if we're primary registered device, as this is only for secondary devices

        let credentials = self.credentials()?;
        let service_configuration: ServiceConfiguration = (*signal_servers).into();

        let push_service = HyperPushService::new(
            service_configuration.clone(),
            Some(credentials.clone()),
            crate::USER_AGENT.to_string(),
        );

        let mut receiver = MessageReceiver::new(push_service);

        let pipe = receiver.create_message_pipe(credentials).await?;
        Ok(pipe.stream())
    }

    pub async fn receive_messages_stream(&self) -> Result<impl Stream<Item = Content> + '_, Error> {
        let encrypted_stream = self.receive_messages_encrypted_stream().await?;
        let signal_servers = match &self.state {
            State::New | State::Registration { .. } => return Err(Error::NotYetRegisteredError),
            State::Registered { signal_servers, .. } => signal_servers,
        };

        let messages = encrypted_stream.filter_map(move |step| async move {
            // TODO: we need to figure out a way to reuse the cipher?
            let service_configuration: ServiceConfiguration = (*signal_servers).into();
            let certificate_validator = service_configuration.credentials_validator().unwrap();
            let mut service_cipher = ServiceCipher::new(
                self.session_store.clone(),
                self.identity_key_store.clone(),
                self.signed_pre_key_store.clone(),
                self.pre_key_store.clone(),
                self.csprng.clone(),
                certificate_validator,
            );
            match step {
                Ok(envelope) => match service_cipher.open_envelope(envelope).await {
                    Ok(Some(content)) => Some(content),
                    Ok(None) => {
                        warn!("Empty envelope...");
                        None
                    }
                    Err(e) => {
                        error!("Error opening envelope: {:?}, message will be skipped!", e);
                        None
                    }
                },
                Err(e) => {
                    error!("Error: {}", e);
                    None
                }
            }
        });
        Ok(messages)
    }

    pub async fn receive_messages(
        &self,
        mut tx: Sender<(Metadata, ContentBody)>,
    ) -> Result<(), Error> {
        let signal_servers = match &self.state {
            State::New | State::Registration { .. } => return Err(Error::NotYetRegisteredError),
            State::Registered { signal_servers, .. } => signal_servers,
        };

        // TODO: error if we're primary registered device, as this is only for secondary devices

        let credentials = self.credentials()?;
        let service_configuration: ServiceConfiguration = (*signal_servers).into();
        let certificate_validator = service_configuration.credentials_validator()?;

        let mut service_cipher = ServiceCipher::new(
            self.config_store.clone(),
            self.config_store.clone(),
            self.config_store.clone(),
            self.config_store.clone(),
            self.csprng.clone(),
            certificate_validator,
        );

        let push_service = HyperPushService::new(
            service_configuration.clone(),
            Some(credentials.clone()),
            crate::USER_AGENT.to_string(),
        );

        let mut receiver = MessageReceiver::new(push_service);

        let pipe = receiver.create_message_pipe(credentials).await.unwrap();
        let message_stream = pipe.stream();
        pin_mut!(message_stream);

        while let Some(step) = message_stream.next().await {
            match step {
                Ok(envelope) => {
                    let Content { body, metadata } =
                        match service_cipher.open_envelope(envelope).await {
                            Ok(Some(content)) => content,
                            Ok(None) => {
                                warn!("Empty envelope...");
                                continue;
                            }
                            Err(e) => {
                                error!("Error opening envelope: {:?}, message will be skipped!", e);
                                continue;
                            }
                        };

                    match &body {
                        ContentBody::SynchronizeMessage(SyncMessage {
                            contacts: Some(contacts),
                            ..
                        }) => {
                            // TODO: save contacts here, for now we just print them
                            let contacts: Result<Vec<Contact>, _> =
                                receiver.retrieve_contacts(contacts).await?.collect();
                            for c in contacts? {
                                log::info!("Contact {}", c.name);
                            }
                            // let _ = cdn_push_service.get_contacts(contacts).await;
                        }
                        _ => tx.send((metadata, body)).await.expect("tx channel error"),
                    };
                }
                Err(e) => {
                    error!("Error: {}", e);
                }
            }
        }

        Ok(())
    }

    pub async fn send_message(
        &self,
        recipient_addr: impl Into<ServiceAddress>,
        message: impl Into<ContentBody>,
        timestamp: u64,
    ) -> Result<(), Error> {
        let mut sender = self.get_sender()?;

        let online_only = false;
        sender
            .send_message(
                &recipient_addr.into(),
                None,
                message,
                timestamp,
                online_only,
            )
            .await?;

        Ok(())
    }

    pub async fn send_message_to_group(
        &self,
        recipients: impl IntoIterator<Item = ServiceAddress>,
        message: DataMessage,
        timestamp: u64,
    ) -> Result<(), Error> {
        let mut sender = self.get_sender()?;

        let recipients: Vec<_> = recipients.into_iter().collect();

        let online_only = false;
        let results = sender
            .send_message_to_group(recipients, None, message, timestamp, online_only)
            .await;

        // return first error if any
        results.into_iter().find(|res| res.is_err()).transpose()?;

        Ok(())
    }

    fn get_sender(&self) -> Result<MessageSender<HyperPushService, S, I, SP, P, R>, Error> {
        let (signal_servers, phone_number, uuid, device_id) = match &self.state {
            State::New | State::Registration { .. } => return Err(Error::NotYetRegisteredError),
            State::Registered {
                signal_servers,
                phone_number,
                uuid,
                device_id,
                ..
            } => (signal_servers, phone_number, uuid, device_id),
        };

        let credentials = self.credentials()?;
        let service_configuration: ServiceConfiguration = (*signal_servers).into();

        let certificate_validator = service_configuration.credentials_validator()?;
        let push_service = HyperPushService::new(
            service_configuration,
            Some(credentials),
            crate::USER_AGENT.to_string(),
        );

        let local_addr = ServiceAddress {
            uuid: Some(*uuid),
            phonenumber: Some(phone_number.clone()),
            relay: None,
        };

        let service_cipher = ServiceCipher::new(
            self.session_store.clone(),
            self.identity_key_store.clone(),
            self.signed_pre_key_store.clone(),
            self.pre_key_store.clone(),
            self.csprng.clone(),
            certificate_validator,
        );

        Ok(MessageSender::new(
            push_service,
            service_cipher,
            self.csprng.clone(),
            self.session_store.clone(),
            self.identity_key_store.clone(),
            local_addr,
            device_id.unwrap_or(DEFAULT_DEVICE_ID),
        ))
    }

    pub fn clear_sessions(&self, recipient: &ServiceAddress) -> Result<(), Error> {
        self.config_store
            .delete_all_sessions(&recipient.identifier())?;
        Ok(())
    }

    pub async fn get_group_v2(
        &mut self,
        group_master_key: GroupMasterKey,
    ) -> Result<libsignal_service::proto::DecryptedGroup, Error> {
        let (signal_servers, _phone_number, uuid, _device_id) = match &self.state {
            State::New | State::Registration { .. } => return Err(Error::NotYetRegisteredError),
            State::Registered {
                signal_servers,
                phone_number,
                uuid,
                device_id,
                ..
            } => (signal_servers, phone_number, uuid, device_id),
        };

        let credentials = self.credentials()?;

        let service_configuration: ServiceConfiguration = (*signal_servers).into();
        let server_public_params = service_configuration.zkgroup_server_public_params;

        let push_service = HyperPushService::new(
            service_configuration,
            Some(credentials),
            crate::USER_AGENT.to_string(),
        );

        let mut groups_v2_credentials_cache = InMemoryCredentialsCache::default();
        let mut groups_v2_api = GroupsManager::new(
            push_service,
            &mut groups_v2_credentials_cache,
            server_public_params,
        );

        let group_secret_params = GroupSecretParams::derive_from_master_key(group_master_key);
        let authorization = groups_v2_api
            .get_authorization_for_today(*uuid, group_secret_params)
            .await?;

        Ok(groups_v2_api
            .get_group(group_secret_params, authorization)
            .await?)
    }
}
