use crate::model::{PlaylistClusterBouquetDto, XtreamCluster};

pub const TARGET_BOUQUET_VERSION: u8 = 2;
pub const TARGET_BOUQUET_SELECTION_VERSION: u8 = 3;

/// An observed category. Names are raw, case-sensitive playlist group names; the
/// cluster disambiguates groups with the same name in Live/VOD/Series.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TargetBouquetCategorySelectionDto {
    pub cluster: XtreamCluster,
    pub name: String,
    /// Lowercase 64-character hex from `item.header.uuid.to_string()`.
    pub included_channel_ids: Vec<String>,
}

/// Snapshot-backed visual selection: missing categories are allowed by default;
/// known categories allow only the explicitly included channel IDs.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TargetBouquetSelectionDto {
    pub categories: Vec<TargetBouquetCategorySelectionDto>,
    /// The non-visual filter the user had before the first visual save (e.g.
    /// `!NO_24H! AND !NO_ADULT!`). The generated `NOT (...)` expression is
    /// rebuilt from this base on every save so the persisted target filter
    /// stays constant-sized instead of nesting the previous compilation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_filter: Option<String>,
}

impl TargetBouquetSelectionDto {
    pub fn canonicalize(&mut self) {
        for category in &mut self.categories {
            category.included_channel_ids.sort_unstable();
            category.included_channel_ids.dedup();
        }
        let mut unique = Vec::<TargetBouquetCategorySelectionDto>::new();
        for category in self.categories.drain(..) {
            if let Some(existing) = unique.iter_mut().find(|item| item.cluster == category.cluster && item.name == category.name) {
                existing.included_channel_ids.extend(category.included_channel_ids);
                existing.included_channel_ids.sort_unstable();
                existing.included_channel_ids.dedup();
            } else {
                unique.push(category);
            }
        }
        self.categories = unique;
    }

    pub fn allows(&self, cluster: XtreamCluster, name: &str, channel_id: &str) -> bool {
        self.categories.iter().find(|item| item.cluster == cluster && item.name == name)
            .is_none_or(|category| category.included_channel_ids.iter().any(|id| id == channel_id))
    }

    /// Merge a fresh inventory snapshot after a successful source update. New
    /// categories start selected; previously known categories keep their exact
    /// included IDs, so newly inserted channels there remain unselected.
    pub fn merge_snapshot(&mut self, categories: impl IntoIterator<Item = TargetBouquetCategorySelectionDto>) {
        self.canonicalize();
        for category in categories {
            if !self.categories.iter().any(|existing| existing.cluster == category.cluster && existing.name == category.name) {
                self.categories.push(category);
            }
        }
        self.canonicalize();
    }
}

#[derive(Debug, Default, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetBouquetMode {
    #[default]
    Whitelist,
    Blacklist,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct TargetBouquetDto {
    pub mode: TargetBouquetMode,
    pub groups: PlaylistClusterBouquetDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<TargetBouquetSelectionDto>,
}

impl TargetBouquetDto {
    pub fn new(mode: TargetBouquetMode, mut groups: PlaylistClusterBouquetDto) -> Self {
        groups.canonicalize_for_target();
        Self { mode, groups, selection: None }
    }

    pub fn whitelist(groups: PlaylistClusterBouquetDto) -> Self { Self::new(TargetBouquetMode::Whitelist, groups) }

    pub fn with_selection(mut self, mut selection: TargetBouquetSelectionDto) -> Self {
        selection.canonicalize();
        self.selection = Some(selection);
        self
    }

    #[inline]
    pub fn is_unrestricted(&self) -> bool { self.selection.is_none() && self.groups.is_target_unrestricted() }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TargetBouquetFileDto {
    pub version: u8,
    pub target: String,
    #[serde(flatten)]
    pub bouquet: TargetBouquetDto,
}

impl TargetBouquetFileDto {
    pub fn new(target: impl Into<String>, bouquet: TargetBouquetDto) -> Self {
        let version = if bouquet.selection.is_some() { TARGET_BOUQUET_SELECTION_VERSION } else { TARGET_BOUQUET_VERSION };
        let mut file = Self { version, target: target.into(), bouquet };
        file.canonicalize();
        file
    }

    #[inline]
    pub fn is_unrestricted(&self) -> bool { self.bouquet.is_unrestricted() }

    pub fn canonicalize(&mut self) {
        self.bouquet.groups.canonicalize_for_target();
        if let Some(selection) = &mut self.bouquet.selection {
            selection.canonicalize();
        }
    }

    /// Reject mixed v2/v3 documents; never silently reinterpret visual selection
    /// as a legacy group filter (or vice versa).
    pub fn is_supported_version(&self) -> bool {
        matches!((self.version, self.bouquet.selection.is_some()), (TARGET_BOUQUET_VERSION, false) | (TARGET_BOUQUET_SELECTION_VERSION, true))
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct TargetInventoryChannelDto {
    /// Lowercase 64-character hex from `item.header.uuid.to_string()`.
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TargetBouquetStreamEventDto {
    Selection { bouquet: Option<TargetBouquetDto> },
    InputStarted { input: String },
    InputChunk { input: String, cluster: XtreamCluster, groups: Vec<String>, is_last_for_cluster: bool },
    InputFinished { input: String, groups: usize },
    InputWarning { input: String, message: String },
    /// Full-inventory stream: one bounded chunk of channels for a single
    /// (cluster, group) pair. lets the UI build the complete category/channel
    /// catalog in one pass over the raw inputs instead of one scan per category.
    InventoryChunk { input: String, cluster: XtreamCluster, group: String, channels: Vec<TargetInventoryChannelDto> },
    Complete,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct TargetBouquetStatusDto {
    pub name: String,
    pub mode: Option<TargetBouquetMode>,
    pub group_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_yaml_and_json() {
        let file = TargetBouquetFileDto::new(
            "family",
            TargetBouquetDto::new(
                TargetBouquetMode::Blacklist,
                PlaylistClusterBouquetDto {
                    live: Some(vec!["News".to_string(), "Kids".to_string()]),
                    vod: Some(vec!["Movies".to_string()]),
                    series: None,
                },
            ),
        );

        let json = serde_json::to_string(&file).expect("serialize json");
        let from_json: TargetBouquetFileDto = serde_json::from_str(&json).expect("deserialize json");
        assert_eq!(file, from_json);

        let yaml = serde_saphyr::to_string(&file).expect("serialize yaml");
        let from_yaml: TargetBouquetFileDto = serde_saphyr::from_str(&yaml).expect("deserialize yaml");
        assert_eq!(file, from_yaml);
    }

    #[test]
    fn rejects_unknown_fields() {
        let yaml = "version: 2\ntarget: family\nmode: whitelist\nextra_field: oops\ngroups:\n  live:\n    - Kids\n";
        let result: Result<TargetBouquetFileDto, _> = serde_saphyr::from_str(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_legacy_files_without_an_explicit_mode() {
        let yaml = "version: 1\ntarget: family\ngroups:\n  live:\n    - Kids\n";
        let result: Result<TargetBouquetFileDto, _> = serde_saphyr::from_str(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn canonicalization_sorts_and_deduplicates_without_trimming_or_case_folding() {
        let mut dto = PlaylistClusterBouquetDto {
            live: Some(vec![
                " Zebra".to_string(),
                "news".to_string(),
                "News".to_string(),
                " Zebra".to_string(),
                String::new(),
            ]),
            vod: Some(vec![]),
            series: None,
        };

        dto.canonicalize_for_target();
        let live = dto.live.as_ref().expect("live should be Some");
        assert_eq!(live, &vec!["", " Zebra", "News", "news"]);
        assert_eq!(dto.vod, Some(Vec::new()));
        assert!(dto.series.is_none());
        assert!(!dto.is_target_unrestricted());
    }

    #[test]
    fn uncategorized_empty_string_is_restricted() {
        let mut file = TargetBouquetFileDto {
            version: TARGET_BOUQUET_VERSION,
            target: "test".to_string(),
            bouquet: TargetBouquetDto::whitelist(PlaylistClusterBouquetDto {
                live: Some(vec![String::new()]),
                vod: None,
                series: None,
            }),
        };
        file.canonicalize();
        assert!(!file.is_unrestricted());
        assert_eq!(file.bouquet.groups.live, Some(vec![String::new()]));
    }

    #[test]
    fn all_empty_is_unrestricted() {
        let mut file = TargetBouquetFileDto {
            version: TARGET_BOUQUET_VERSION,
            target: "test".to_string(),
            bouquet: TargetBouquetDto::new(
                TargetBouquetMode::Blacklist,
                PlaylistClusterBouquetDto { live: Some(vec![]), vod: Some(vec![]), series: None },
            ),
        };
        file.canonicalize();
        assert!(file.is_unrestricted());
        assert_eq!(file.bouquet.mode, TargetBouquetMode::Blacklist);
        assert_eq!(file.bouquet.groups.live, None);
        assert_eq!(file.bouquet.groups.vod, None);
        assert_eq!(file.bouquet.groups.series, None);
    }

    #[test]
    fn visual_selection_round_trip_and_snapshot_merge() {
        let known = TargetBouquetCategorySelectionDto {
            cluster: XtreamCluster::Live, name: "News".to_string(),
            included_channel_ids: vec!["a".repeat(64)],
        };
        let file = TargetBouquetFileDto::new("family", TargetBouquetDto::whitelist(PlaylistClusterBouquetDto::default())
            .with_selection(TargetBouquetSelectionDto { categories: vec![known.clone()], base_filter: None }));
        assert_eq!(file.version, TARGET_BOUQUET_SELECTION_VERSION);
        assert!(file.is_supported_version());
        let yaml = serde_saphyr::to_string(&file).unwrap();
        let mut decoded: TargetBouquetFileDto = serde_saphyr::from_str(&yaml).unwrap();
        assert_eq!(file, decoded);
        let selection = decoded.bouquet.selection.as_mut().unwrap();
        assert!(selection.allows(XtreamCluster::Live, "New category", "new"));
        assert!(selection.allows(XtreamCluster::Live, "News", &"a".repeat(64)));
        assert!(!selection.allows(XtreamCluster::Live, "News", &"b".repeat(64)));
        selection.merge_snapshot(vec![
            TargetBouquetCategorySelectionDto { included_channel_ids: vec!["a".repeat(64), "b".repeat(64)], ..known.clone() },
            TargetBouquetCategorySelectionDto { cluster: XtreamCluster::Video, name: "News".to_string(), included_channel_ids: vec!["c".repeat(64)] },
        ]);
        assert!(!selection.allows(XtreamCluster::Live, "News", &"b".repeat(64)));
        assert!(selection.allows(XtreamCluster::Video, "News", &"c".repeat(64)));
        selection.categories[0].included_channel_ids.clear();
        assert!(!selection.allows(XtreamCluster::Live, "News", &"a".repeat(64)));
    }

    #[test]
    fn mismatched_versions_do_not_interpret_legacy_as_visual() {
        let mut file = TargetBouquetFileDto::new("family", TargetBouquetDto::whitelist(PlaylistClusterBouquetDto::default()));
        assert!(file.is_supported_version());
        file.version = TARGET_BOUQUET_SELECTION_VERSION;
        assert!(!file.is_supported_version());
    }

    #[test]
    fn stream_event_serde_round_trip() {
        let event = TargetBouquetStreamEventDto::InputChunk {
            input: "provider_1".to_string(),
            cluster: XtreamCluster::Live,
            groups: vec!["News HD".to_string()],
            is_last_for_cluster: true,
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let parsed: TargetBouquetStreamEventDto = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, parsed);
    }
}
