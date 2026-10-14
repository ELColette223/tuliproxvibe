use shared::model::{TargetBouquetDto, TargetBouquetMode, TargetBouquetSelectionDto, XtreamCluster};
use std::collections::HashSet;

/// Target-local compiled bouquet filter evaluated per item before transformation.
#[derive(Debug, Clone, PartialEq)]
pub struct TargetBouquetFilter {
    mode: TargetBouquetMode,
    live: Option<HashSet<String>>,
    vod: Option<HashSet<String>>,
    series: Option<HashSet<String>>,
    selection: Option<TargetBouquetSelectionDto>,
}

impl TargetBouquetFilter {
    pub fn from_dto(mut bouquet: TargetBouquetDto) -> Option<Self> {
        bouquet.groups.canonicalize_for_target();
        if let Some(selection) = &mut bouquet.selection {
            selection.canonicalize();
        }
        if bouquet.is_unrestricted() {
            return None;
        }
        let live = bouquet.groups.live.map(|v| v.into_iter().collect::<HashSet<String>>());
        let vod = bouquet.groups.vod.map(|v| v.into_iter().collect::<HashSet<String>>());
        let series = bouquet.groups.series.map(|v| v.into_iter().collect::<HashSet<String>>());
        Some(Self { mode: bouquet.mode, live, vod, series, selection: bouquet.selection })
    }

    #[inline]
    fn for_cluster(&self, cluster: XtreamCluster) -> Option<&HashSet<String>> {
        match cluster {
            XtreamCluster::Live => self.live.as_ref(),
            XtreamCluster::Video => self.vod.as_ref(),
            XtreamCluster::Series => self.series.as_ref(),
        }
    }

    #[inline]
    pub fn allows(&self, cluster: XtreamCluster, group: &str) -> bool {
        let groups = self.for_cluster(cluster);
        match self.mode {
            TargetBouquetMode::Whitelist => groups.is_none_or(|groups| groups.contains(group)),
            TargetBouquetMode::Blacklist => groups.is_none_or(|groups| !groups.contains(group)),
        }
    }

    /// Use the origin UUID before transformations, formatted via
    /// `item.header.uuid.to_string()` (64 lowercase hexadecimal characters).
    pub fn allows_channel(&self, cluster: XtreamCluster, group: &str, channel_id: &str) -> bool {
        self.selection.as_ref().map_or_else(|| self.allows(cluster, group), |selection| selection.allows(cluster, group, channel_id))
    }

    pub fn cluster_counts(&self) -> (Option<usize>, Option<usize>, Option<usize>) {
        (
            self.live.as_ref().map(HashSet::len),
            self.vod.as_ref().map(HashSet::len),
            self.series.as_ref().map(HashSet::len),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::PlaylistClusterBouquetDto;

    fn whitelist(groups: PlaylistClusterBouquetDto) -> TargetBouquetDto {
        TargetBouquetDto::new(TargetBouquetMode::Whitelist, groups)
    }

    #[test]
    fn allows_all_when_cluster_is_none() {
        let filter = TargetBouquetFilter::from_dto(whitelist(PlaylistClusterBouquetDto {
            live: Some(vec!["News".to_string()]),
            vod: None,
            series: None,
        }))
        .expect("filter should be Some");

        assert!(filter.allows(XtreamCluster::Live, "News"));
        assert!(!filter.allows(XtreamCluster::Live, "Sports"));
        assert!(filter.allows(XtreamCluster::Video, "Any Movie"));
        assert!(filter.allows(XtreamCluster::Series, "Any Series"));
    }

    #[test]
    fn case_sensitive_exact_matching() {
        let filter = TargetBouquetFilter::from_dto(whitelist(PlaylistClusterBouquetDto {
            live: Some(vec!["news".to_string(), String::new()]),
            vod: None,
            series: None,
        }))
        .expect("filter should be Some");

        assert!(filter.allows(XtreamCluster::Live, "news"));
        assert!(!filter.allows(XtreamCluster::Live, "News"));
        assert!(filter.allows(XtreamCluster::Live, ""));
    }

    #[test]
    fn returns_none_when_unrestricted() {
        let filter =
            TargetBouquetFilter::from_dto(whitelist(PlaylistClusterBouquetDto { live: None, vod: None, series: None }));
        assert!(filter.is_none());

        let filter_empty = TargetBouquetFilter::from_dto(whitelist(PlaylistClusterBouquetDto {
            live: Some(vec![]),
            vod: None,
            series: None,
        }));
        assert!(filter_empty.is_none());
    }

    #[test]
    fn empty_cluster_excludes_that_cluster_when_another_cluster_is_selected() {
        let filter = TargetBouquetFilter::from_dto(whitelist(PlaylistClusterBouquetDto {
            live: Some(vec!["News".to_string()]),
            vod: Some(vec![]),
            series: None,
        }))
        .expect("filter should be Some");

        assert!(filter.allows(XtreamCluster::Live, "News"));
        assert!(!filter.allows(XtreamCluster::Live, "Sports"));
        assert!(!filter.allows(XtreamCluster::Video, "Any Movie"));
    }

    #[test]
    fn visual_selection_known_empty_and_unknown_default() {
        let selection = TargetBouquetSelectionDto { categories: vec![shared::model::TargetBouquetCategorySelectionDto {
            cluster: XtreamCluster::Live, name: "News".to_string(), included_channel_ids: vec![],
        }] };
        let filter = TargetBouquetFilter::from_dto(whitelist(PlaylistClusterBouquetDto::default()).with_selection(selection)).unwrap();
        assert!(!filter.allows_channel(XtreamCluster::Live, "News", &"a".repeat(64)));
        assert!(filter.allows_channel(XtreamCluster::Live, "New", &"a".repeat(64)));
        assert!(filter.allows_channel(XtreamCluster::Video, "News", &"a".repeat(64)));
    }

    #[test]
    fn blacklist_excludes_selected_groups_and_allows_everything_else() {
        let filter = TargetBouquetFilter::from_dto(TargetBouquetDto::new(
            TargetBouquetMode::Blacklist,
            PlaylistClusterBouquetDto { live: Some(vec!["Sports".to_string()]), vod: Some(Vec::new()), series: None },
        ))
        .expect("filter should be Some");

        assert!(!filter.allows(XtreamCluster::Live, "Sports"));
        assert!(filter.allows(XtreamCluster::Live, "News"));
        assert!(filter.allows(XtreamCluster::Video, "Any Movie"));
        assert!(filter.allows(XtreamCluster::Series, "Any Series"));
    }
}
