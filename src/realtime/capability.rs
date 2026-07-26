//! Exhaustive upstream-profile policy for every Realtime surface.
//!
//! This module answers only whether a classified operation is supported. URL,
//! body, header, credential, timeout, permit, and pump behavior stay with their
//! existing owners.

use crate::config::UpstreamProfile;

use super::contract::{ApiDialect, ClassifiedRest, ClassifiedWebSocket, WebSocketTarget};
use super::path::RestOperation;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProfileKind {
    ApiKeyManaged,
    ApiKeyClient,
    ChatGpt,
}

impl ProfileKind {
    pub const ALL: [Self; 3] = [Self::ApiKeyManaged, Self::ApiKeyClient, Self::ChatGpt];

    pub const fn label(self) -> &'static str {
        match self {
            Self::ApiKeyManaged => "apikey_managed",
            Self::ApiKeyClient => "apikey_client",
            Self::ChatGpt => "chatgpt",
        }
    }

    pub fn from_profile(profile: &UpstreamProfile) -> Self {
        match profile {
            UpstreamProfile::ApiKeyManaged { .. } => Self::ApiKeyManaged,
            UpstreamProfile::ApiKeyClient { .. } => Self::ApiKeyClient,
            UpstreamProfile::ChatGptBackend { .. } => Self::ChatGpt,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OfficialRestCapability {
    CreateCall,
    AcceptCall,
    RejectCall,
    ReferCall,
    HangupCall,
    CreateClientSecret,
    CreateLegacySession,
    CreateTranscriptionSession,
    CreateTranslationClientSecret,
    CreateTranslationCall,
}

impl OfficialRestCapability {
    pub const ALL: [Self; 10] = [
        Self::CreateCall,
        Self::AcceptCall,
        Self::RejectCall,
        Self::ReferCall,
        Self::HangupCall,
        Self::CreateClientSecret,
        Self::CreateLegacySession,
        Self::CreateTranscriptionSession,
        Self::CreateTranslationClientSecret,
        Self::CreateTranslationCall,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::CreateCall => "official_webrtc_call_create",
            Self::AcceptCall => "official_call_accept",
            Self::RejectCall => "official_call_reject",
            Self::ReferCall => "official_call_refer",
            Self::HangupCall => "official_call_hangup",
            Self::CreateClientSecret => "official_realtime_client_secret",
            Self::CreateLegacySession => "official_legacy_session_token",
            Self::CreateTranscriptionSession => "official_transcription_session_token",
            Self::CreateTranslationClientSecret => "official_translation_client_secret",
            Self::CreateTranslationCall => "official_translation_webrtc_call",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OfficialWebSocketCapability {
    Standalone,
    ExistingCall,
    Translation,
}

impl OfficialWebSocketCapability {
    pub const ALL: [Self; 3] = [Self::Standalone, Self::ExistingCall, Self::Translation];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Standalone => "official_standalone_websocket",
            Self::ExistingCall => "official_existing_call_websocket",
            Self::Translation => "official_translation_websocket",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    OfficialRest(OfficialRestCapability),
    OfficialWebSocket(OfficialWebSocketCapability),
    PrivateCallCreate(ApiDialect),
    PrivateStandaloneWebSocket(ApiDialect),
    PrivateExistingCallWebSocket(ApiDialect),
    PrivateSidebandAlias(ApiDialect),
}

impl Capability {
    pub const ALL: [Self; 21] = [
        Self::OfficialRest(OfficialRestCapability::CreateCall),
        Self::OfficialRest(OfficialRestCapability::AcceptCall),
        Self::OfficialRest(OfficialRestCapability::RejectCall),
        Self::OfficialRest(OfficialRestCapability::ReferCall),
        Self::OfficialRest(OfficialRestCapability::HangupCall),
        Self::OfficialRest(OfficialRestCapability::CreateClientSecret),
        Self::OfficialRest(OfficialRestCapability::CreateLegacySession),
        Self::OfficialRest(OfficialRestCapability::CreateTranscriptionSession),
        Self::OfficialRest(OfficialRestCapability::CreateTranslationClientSecret),
        Self::OfficialRest(OfficialRestCapability::CreateTranslationCall),
        Self::OfficialWebSocket(OfficialWebSocketCapability::Standalone),
        Self::OfficialWebSocket(OfficialWebSocketCapability::ExistingCall),
        Self::OfficialWebSocket(OfficialWebSocketCapability::Translation),
        Self::PrivateCallCreate(ApiDialect::QuicksilverV1),
        Self::PrivateCallCreate(ApiDialect::Frameless),
        Self::PrivateStandaloneWebSocket(ApiDialect::QuicksilverV1),
        Self::PrivateStandaloneWebSocket(ApiDialect::Frameless),
        Self::PrivateExistingCallWebSocket(ApiDialect::QuicksilverV1),
        Self::PrivateExistingCallWebSocket(ApiDialect::Frameless),
        Self::PrivateSidebandAlias(ApiDialect::QuicksilverV1),
        Self::PrivateSidebandAlias(ApiDialect::Frameless),
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::OfficialRest(capability) => capability.label(),
            Self::OfficialWebSocket(capability) => capability.label(),
            Self::PrivateCallCreate(ApiDialect::QuicksilverV1) => "private_v1_call_create",
            Self::PrivateCallCreate(ApiDialect::Frameless) => "private_frameless_call_create",
            Self::PrivateStandaloneWebSocket(ApiDialect::QuicksilverV1) => {
                "private_v1_standalone_websocket"
            }
            Self::PrivateStandaloneWebSocket(ApiDialect::Frameless) => {
                "private_frameless_standalone_websocket"
            }
            Self::PrivateExistingCallWebSocket(ApiDialect::QuicksilverV1) => {
                "private_v1_existing_call_websocket"
            }
            Self::PrivateExistingCallWebSocket(ApiDialect::Frameless) => {
                "private_frameless_existing_call_websocket"
            }
            Self::PrivateSidebandAlias(ApiDialect::QuicksilverV1) => "private_v1_sideband_alias",
            Self::PrivateSidebandAlias(ApiDialect::Frameless) => "private_frameless_sideband_alias",
            Self::PrivateCallCreate(ApiDialect::OfficialGa)
            | Self::PrivateStandaloneWebSocket(ApiDialect::OfficialGa)
            | Self::PrivateExistingCallWebSocket(ApiDialect::OfficialGa)
            | Self::PrivateSidebandAlias(ApiDialect::OfficialGa) => {
                "invalid_private_official_capability"
            }
        }
    }

    pub fn from_rest(classified: &ClassifiedRest) -> Self {
        if classified.selection.dialect != ApiDialect::OfficialGa {
            debug_assert!(matches!(classified.operation, RestOperation::CreateCall));
            return Self::PrivateCallCreate(classified.selection.dialect);
        }
        Self::OfficialRest(match classified.operation {
            RestOperation::CreateCall => OfficialRestCapability::CreateCall,
            RestOperation::AcceptCall { .. } => OfficialRestCapability::AcceptCall,
            RestOperation::RejectCall { .. } => OfficialRestCapability::RejectCall,
            RestOperation::ReferCall { .. } => OfficialRestCapability::ReferCall,
            RestOperation::HangupCall { .. } => OfficialRestCapability::HangupCall,
            RestOperation::CreateClientSecret => OfficialRestCapability::CreateClientSecret,
            RestOperation::CreateLegacySession => OfficialRestCapability::CreateLegacySession,
            RestOperation::CreateTranscriptionSession => {
                OfficialRestCapability::CreateTranscriptionSession
            }
            RestOperation::CreateTranslationClientSecret => {
                OfficialRestCapability::CreateTranslationClientSecret
            }
            RestOperation::CreateTranslationCall => OfficialRestCapability::CreateTranslationCall,
        })
    }

    pub fn from_websocket(classified: &ClassifiedWebSocket) -> Self {
        match (classified.selection.dialect, &classified.target) {
            (ApiDialect::OfficialGa, WebSocketTarget::Standalone { .. }) => {
                Self::OfficialWebSocket(OfficialWebSocketCapability::Standalone)
            }
            (ApiDialect::OfficialGa, WebSocketTarget::ExistingCall { .. }) => {
                Self::OfficialWebSocket(OfficialWebSocketCapability::ExistingCall)
            }
            (ApiDialect::OfficialGa, WebSocketTarget::Translation { .. }) => {
                Self::OfficialWebSocket(OfficialWebSocketCapability::Translation)
            }
            (dialect, WebSocketTarget::Standalone { .. }) => {
                Self::PrivateStandaloneWebSocket(dialect)
            }
            (dialect, WebSocketTarget::ExistingCall { .. }) => {
                Self::PrivateExistingCallWebSocket(dialect)
            }
            (_, WebSocketTarget::Translation { .. }) => {
                unreachable!("private translation is rejected during classification")
            }
        }
    }

    pub const fn private_sideband_alias(dialect: ApiDialect) -> Self {
        Self::PrivateSidebandAlias(dialect)
    }

    const fn is_official(self) -> bool {
        matches!(self, Self::OfficialRest(_) | Self::OfficialWebSocket(_))
    }

    const fn is_private_standalone(self) -> bool {
        matches!(self, Self::PrivateStandaloneWebSocket(_))
    }

    const fn is_private(self) -> bool {
        matches!(
            self,
            Self::PrivateCallCreate(_)
                | Self::PrivateStandaloneWebSocket(_)
                | Self::PrivateExistingCallWebSocket(_)
                | Self::PrivateSidebandAlias(_)
        )
    }
}

const API_KEY_PROFILES: &[ProfileKind] = &[ProfileKind::ApiKeyManaged, ProfileKind::ApiKeyClient];
const MANAGED_PROFILES: &[ProfileKind] = &[ProfileKind::ApiKeyManaged, ProfileKind::ChatGpt];
const API_KEY_MANAGED_PROFILE: &[ProfileKind] = &[ProfileKind::ApiKeyManaged];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    Native,
    Adapted,
    Unsupported {
        required_profiles: &'static [ProfileKind],
    },
}

impl Support {
    pub const fn required_profiles(self) -> Option<&'static [ProfileKind]> {
        match self {
            Self::Native | Self::Adapted => None,
            Self::Unsupported { required_profiles } => Some(required_profiles),
        }
    }
}

pub const fn support(profile: ProfileKind, capability: Capability) -> Support {
    if capability.is_official() {
        return match profile {
            ProfileKind::ApiKeyManaged | ProfileKind::ApiKeyClient => Support::Native,
            ProfileKind::ChatGpt => Support::Unsupported {
                required_profiles: API_KEY_PROFILES,
            },
        };
    }

    debug_assert!(capability.is_private());
    match profile {
        ProfileKind::ApiKeyManaged => Support::Adapted,
        ProfileKind::ApiKeyClient => Support::Unsupported {
            required_profiles: if capability.is_private_standalone() {
                API_KEY_MANAGED_PROFILE
            } else {
                MANAGED_PROFILES
            },
        },
        ProfileKind::ChatGpt if capability.is_private_standalone() => Support::Unsupported {
            required_profiles: API_KEY_MANAGED_PROFILE,
        },
        ProfileKind::ChatGpt => Support::Adapted,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityRow {
    pub profile: ProfileKind,
    pub capability: Capability,
    pub support: Support,
}

const TABLE_LEN: usize = ProfileKind::ALL.len() * Capability::ALL.len();

const fn build_table() -> [CapabilityRow; TABLE_LEN] {
    let placeholder = CapabilityRow {
        profile: ProfileKind::ApiKeyManaged,
        capability: Capability::ALL[0],
        support: Support::Native,
    };
    let mut rows = [placeholder; TABLE_LEN];
    let mut profile_index = 0;
    while profile_index < ProfileKind::ALL.len() {
        let mut capability_index = 0;
        while capability_index < Capability::ALL.len() {
            let profile = ProfileKind::ALL[profile_index];
            let capability = Capability::ALL[capability_index];
            rows[profile_index * Capability::ALL.len() + capability_index] = CapabilityRow {
                profile,
                capability,
                support: support(profile, capability),
            };
            capability_index += 1;
        }
        profile_index += 1;
    }
    rows
}

pub const CAPABILITY_TABLE: [CapabilityRow; TABLE_LEN] = build_table();

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    const EXPECTED_API_KEY_PROFILES: &[ProfileKind] =
        &[ProfileKind::ApiKeyManaged, ProfileKind::ApiKeyClient];
    const EXPECTED_MANAGED_PROFILES: &[ProfileKind] =
        &[ProfileKind::ApiKeyManaged, ProfileKind::ChatGpt];
    const EXPECTED_API_KEY_MANAGED_PROFILE: &[ProfileKind] = &[ProfileKind::ApiKeyManaged];

    const fn expected_row(
        profile: ProfileKind,
        capability: Capability,
        support: Support,
    ) -> CapabilityRow {
        CapabilityRow {
            profile,
            capability,
            support,
        }
    }

    const EXPECTED_ROWS: [CapabilityRow; 63] = [
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::OfficialRest(OfficialRestCapability::CreateCall),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::OfficialRest(OfficialRestCapability::AcceptCall),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::OfficialRest(OfficialRestCapability::RejectCall),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::OfficialRest(OfficialRestCapability::ReferCall),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::OfficialRest(OfficialRestCapability::HangupCall),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::OfficialRest(OfficialRestCapability::CreateClientSecret),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::OfficialRest(OfficialRestCapability::CreateLegacySession),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::OfficialRest(OfficialRestCapability::CreateTranscriptionSession),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::OfficialRest(OfficialRestCapability::CreateTranslationClientSecret),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::OfficialRest(OfficialRestCapability::CreateTranslationCall),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::OfficialWebSocket(OfficialWebSocketCapability::Standalone),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::OfficialWebSocket(OfficialWebSocketCapability::ExistingCall),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::OfficialWebSocket(OfficialWebSocketCapability::Translation),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::PrivateCallCreate(ApiDialect::QuicksilverV1),
            Support::Adapted,
        ),
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::PrivateCallCreate(ApiDialect::Frameless),
            Support::Adapted,
        ),
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::PrivateStandaloneWebSocket(ApiDialect::QuicksilverV1),
            Support::Adapted,
        ),
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::PrivateStandaloneWebSocket(ApiDialect::Frameless),
            Support::Adapted,
        ),
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::PrivateExistingCallWebSocket(ApiDialect::QuicksilverV1),
            Support::Adapted,
        ),
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::PrivateExistingCallWebSocket(ApiDialect::Frameless),
            Support::Adapted,
        ),
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::PrivateSidebandAlias(ApiDialect::QuicksilverV1),
            Support::Adapted,
        ),
        expected_row(
            ProfileKind::ApiKeyManaged,
            Capability::PrivateSidebandAlias(ApiDialect::Frameless),
            Support::Adapted,
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::OfficialRest(OfficialRestCapability::CreateCall),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::OfficialRest(OfficialRestCapability::AcceptCall),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::OfficialRest(OfficialRestCapability::RejectCall),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::OfficialRest(OfficialRestCapability::ReferCall),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::OfficialRest(OfficialRestCapability::HangupCall),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::OfficialRest(OfficialRestCapability::CreateClientSecret),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::OfficialRest(OfficialRestCapability::CreateLegacySession),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::OfficialRest(OfficialRestCapability::CreateTranscriptionSession),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::OfficialRest(OfficialRestCapability::CreateTranslationClientSecret),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::OfficialRest(OfficialRestCapability::CreateTranslationCall),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::OfficialWebSocket(OfficialWebSocketCapability::Standalone),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::OfficialWebSocket(OfficialWebSocketCapability::ExistingCall),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::OfficialWebSocket(OfficialWebSocketCapability::Translation),
            Support::Native,
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::PrivateCallCreate(ApiDialect::QuicksilverV1),
            Support::Unsupported {
                required_profiles: EXPECTED_MANAGED_PROFILES,
            },
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::PrivateCallCreate(ApiDialect::Frameless),
            Support::Unsupported {
                required_profiles: EXPECTED_MANAGED_PROFILES,
            },
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::PrivateStandaloneWebSocket(ApiDialect::QuicksilverV1),
            Support::Unsupported {
                required_profiles: EXPECTED_API_KEY_MANAGED_PROFILE,
            },
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::PrivateStandaloneWebSocket(ApiDialect::Frameless),
            Support::Unsupported {
                required_profiles: EXPECTED_API_KEY_MANAGED_PROFILE,
            },
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::PrivateExistingCallWebSocket(ApiDialect::QuicksilverV1),
            Support::Unsupported {
                required_profiles: EXPECTED_MANAGED_PROFILES,
            },
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::PrivateExistingCallWebSocket(ApiDialect::Frameless),
            Support::Unsupported {
                required_profiles: EXPECTED_MANAGED_PROFILES,
            },
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::PrivateSidebandAlias(ApiDialect::QuicksilverV1),
            Support::Unsupported {
                required_profiles: EXPECTED_MANAGED_PROFILES,
            },
        ),
        expected_row(
            ProfileKind::ApiKeyClient,
            Capability::PrivateSidebandAlias(ApiDialect::Frameless),
            Support::Unsupported {
                required_profiles: EXPECTED_MANAGED_PROFILES,
            },
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::OfficialRest(OfficialRestCapability::CreateCall),
            Support::Unsupported {
                required_profiles: EXPECTED_API_KEY_PROFILES,
            },
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::OfficialRest(OfficialRestCapability::AcceptCall),
            Support::Unsupported {
                required_profiles: EXPECTED_API_KEY_PROFILES,
            },
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::OfficialRest(OfficialRestCapability::RejectCall),
            Support::Unsupported {
                required_profiles: EXPECTED_API_KEY_PROFILES,
            },
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::OfficialRest(OfficialRestCapability::ReferCall),
            Support::Unsupported {
                required_profiles: EXPECTED_API_KEY_PROFILES,
            },
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::OfficialRest(OfficialRestCapability::HangupCall),
            Support::Unsupported {
                required_profiles: EXPECTED_API_KEY_PROFILES,
            },
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::OfficialRest(OfficialRestCapability::CreateClientSecret),
            Support::Unsupported {
                required_profiles: EXPECTED_API_KEY_PROFILES,
            },
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::OfficialRest(OfficialRestCapability::CreateLegacySession),
            Support::Unsupported {
                required_profiles: EXPECTED_API_KEY_PROFILES,
            },
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::OfficialRest(OfficialRestCapability::CreateTranscriptionSession),
            Support::Unsupported {
                required_profiles: EXPECTED_API_KEY_PROFILES,
            },
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::OfficialRest(OfficialRestCapability::CreateTranslationClientSecret),
            Support::Unsupported {
                required_profiles: EXPECTED_API_KEY_PROFILES,
            },
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::OfficialRest(OfficialRestCapability::CreateTranslationCall),
            Support::Unsupported {
                required_profiles: EXPECTED_API_KEY_PROFILES,
            },
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::OfficialWebSocket(OfficialWebSocketCapability::Standalone),
            Support::Unsupported {
                required_profiles: EXPECTED_API_KEY_PROFILES,
            },
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::OfficialWebSocket(OfficialWebSocketCapability::ExistingCall),
            Support::Unsupported {
                required_profiles: EXPECTED_API_KEY_PROFILES,
            },
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::OfficialWebSocket(OfficialWebSocketCapability::Translation),
            Support::Unsupported {
                required_profiles: EXPECTED_API_KEY_PROFILES,
            },
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::PrivateCallCreate(ApiDialect::QuicksilverV1),
            Support::Adapted,
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::PrivateCallCreate(ApiDialect::Frameless),
            Support::Adapted,
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::PrivateStandaloneWebSocket(ApiDialect::QuicksilverV1),
            Support::Unsupported {
                required_profiles: EXPECTED_API_KEY_MANAGED_PROFILE,
            },
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::PrivateStandaloneWebSocket(ApiDialect::Frameless),
            Support::Unsupported {
                required_profiles: EXPECTED_API_KEY_MANAGED_PROFILE,
            },
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::PrivateExistingCallWebSocket(ApiDialect::QuicksilverV1),
            Support::Adapted,
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::PrivateExistingCallWebSocket(ApiDialect::Frameless),
            Support::Adapted,
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::PrivateSidebandAlias(ApiDialect::QuicksilverV1),
            Support::Adapted,
        ),
        expected_row(
            ProfileKind::ChatGpt,
            Capability::PrivateSidebandAlias(ApiDialect::Frameless),
            Support::Adapted,
        ),
    ];

    #[test]
    fn table_and_support_match_independent_exact_oracle() {
        assert_eq!(CAPABILITY_TABLE, EXPECTED_ROWS);
        for expected in EXPECTED_ROWS {
            assert_eq!(
                support(expected.profile, expected.capability),
                expected.support,
                "unexpected support for profile={} capability={}",
                expected.profile.label(),
                expected.capability.label(),
            );
        }
    }

    #[test]
    fn independent_oracle_covers_every_declared_capability_for_every_profile() {
        let pairs: HashSet<_> = EXPECTED_ROWS
            .iter()
            .map(|row| (row.profile, row.capability))
            .collect();
        assert_eq!(pairs.len(), EXPECTED_ROWS.len());
        for profile in ProfileKind::ALL {
            for capability in Capability::ALL {
                assert!(
                    pairs.contains(&(profile, capability)),
                    "missing oracle row for profile={} capability={}",
                    profile.label(),
                    capability.label(),
                );
            }
        }
    }

    #[test]
    fn table_has_exactly_one_row_for_every_profile_capability_pair() {
        let pairs: HashSet<_> = CAPABILITY_TABLE
            .iter()
            .map(|row| (row.profile, row.capability))
            .collect();
        assert_eq!(CAPABILITY_TABLE.len(), 63);
        assert_eq!(pairs.len(), CAPABILITY_TABLE.len());
        for profile in ProfileKind::ALL {
            for capability in Capability::ALL {
                assert!(pairs.contains(&(profile, capability)));
            }
        }
    }

    #[test]
    fn native_is_reserved_for_official_api_key_surfaces() {
        for row in CAPABILITY_TABLE {
            if row.support == Support::Native {
                assert!(row.capability.is_official());
                assert_ne!(row.profile, ProfileKind::ChatGpt);
            }
            if row.capability.is_private() {
                assert_ne!(row.support, Support::Native);
            }
        }
    }

    #[test]
    fn all_declared_sub_capabilities_are_present_in_all() {
        for capability in OfficialRestCapability::ALL {
            assert!(Capability::ALL.contains(&Capability::OfficialRest(capability)));
        }
        for capability in OfficialWebSocketCapability::ALL {
            assert!(Capability::ALL.contains(&Capability::OfficialWebSocket(capability)));
        }
    }
}
