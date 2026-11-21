//! CDN preference and ordered candidates for Bilibili media streams.

use feedbot_core::extract::DownloadSource;
use std::str::FromStr;
use url::Url;

const MAX_STREAM_URLS: usize = 3;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BilibiliCdn {
    #[default]
    BaseUrl,
    BackupUrl,
    Mirror(&'static str),
}

impl FromStr for BilibiliCdn {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let cdn = match value.trim().to_ascii_lowercase().as_str() {
            "baseurl" | "base" => Self::BaseUrl,
            "backupurl" | "backup" => Self::BackupUrl,
            "ali" => Self::Mirror("upos-sz-mirrorali.bilivideo.com"),
            "alib" => Self::Mirror("upos-sz-mirroralib.bilivideo.com"),
            "alio1" => Self::Mirror("upos-sz-mirroralio1.bilivideo.com"),
            "cos" => Self::Mirror("upos-sz-mirrorcos.bilivideo.com"),
            "cosb" => Self::Mirror("upos-sz-mirrorcosb.bilivideo.com"),
            "coso1" => Self::Mirror("upos-sz-mirrorcoso1.bilivideo.com"),
            "hw" => Self::Mirror("upos-sz-mirrorhw.bilivideo.com"),
            "hwb" => Self::Mirror("upos-sz-mirrorhwb.bilivideo.com"),
            "hwo1" => Self::Mirror("upos-sz-mirrorhwo1.bilivideo.com"),
            "08c" | "hw_08c" => Self::Mirror("upos-sz-mirror08c.bilivideo.com"),
            "08h" | "hw_08h" => Self::Mirror("upos-sz-mirror08h.bilivideo.com"),
            "08ct" | "hw_08ct" => Self::Mirror("upos-sz-mirror08ct.bilivideo.com"),
            "tf_hw" => Self::Mirror("upos-tf-all-hw.bilivideo.com"),
            "tf_tx" => Self::Mirror("upos-tf-all-tx.bilivideo.com"),
            "akamai" => Self::Mirror("upos-hz-mirrorakam.akamaized.net"),
            "aliov" => Self::Mirror("upos-sz-mirroraliov.bilivideo.com"),
            "cosov" => Self::Mirror("upos-sz-mirrorcosov.bilivideo.com"),
            "hwov" => Self::Mirror("upos-sz-mirrorhwov.bilivideo.com"),
            "hk_bcache" => Self::Mirror("cn-hk-eq-bcache-01.bilivideo.com"),
            _ => return Err("Bilibili CDN 仅支持 baseUrl、backupUrl 或已知镜像别名".into()),
        };
        Ok(cdn)
    }
}

impl BilibiliCdn {
    pub(super) fn order(
        self,
        primary: Option<Url>,
        mut backups: impl Iterator<Item = Url>,
    ) -> Option<DownloadSource> {
        let backup_first = self == Self::BackupUrl && primary.is_some();
        let primary = primary.or_else(|| backups.next())?;
        let credential_origin = primary.origin();
        let mirror_host = match self {
            Self::Mirror(host) if primary.host_str() != Some(host) => Some(host),
            _ => None,
        };

        let mut api_mirror = None;
        let mut fallback_urls = Vec::with_capacity(MAX_STREAM_URLS - 1);
        for backup in backups.filter(|url| url != &primary) {
            if let Some(host) = mirror_host
                && api_mirror.is_none()
                && backup.host_str() == Some(host)
            {
                api_mirror = Some(backup.clone());
            }
            if fallback_urls.len() < MAX_STREAM_URLS - 1 && !fallback_urls.contains(&backup) {
                fallback_urls.push(backup);
            }
            // A requested mirror may appear after the normal fallback slots are full.
            if fallback_urls.len() == MAX_STREAM_URLS - 1
                && (mirror_host.is_none() || api_mirror.is_some())
            {
                break;
            }
        }

        let urls = if backup_first {
            fallback_urls.push(primary);
            fallback_urls
        } else {
            let mirror =
                api_mirror.or_else(|| mirror_host.and_then(|host| rewrite_host(&primary, host)));
            let mut urls = Vec::with_capacity(MAX_STREAM_URLS);
            urls.extend(mirror);
            urls.push(primary);
            for fallback in fallback_urls {
                if urls.len() == MAX_STREAM_URLS {
                    break;
                }
                if !urls.contains(&fallback) {
                    urls.push(fallback);
                }
            }
            urls
        };
        Some(DownloadSource {
            urls,
            credential_origin,
        })
    }
}

fn rewrite_host(original: &Url, host: &str) -> Option<Url> {
    let mut mirror = original.clone();
    mirror.set_host(Some(host)).ok()?;
    mirror.set_port(None).ok()?;
    mirror.set_username("").ok()?;
    mirror.set_password(None).ok()?;
    Some(mirror)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    #[test]
    fn parses_legacy_aliases_and_rejects_hosts() {
        assert_eq!(" BaseURL ".parse(), Ok(BilibiliCdn::BaseUrl));
        assert_eq!("BACKUP".parse(), Ok(BilibiliCdn::BackupUrl));
        assert_eq!(
            " HK_BCACHE ".parse(),
            Ok(BilibiliCdn::Mirror("cn-hk-eq-bcache-01.bilivideo.com"))
        );
        assert!("cdn.example.org".parse::<BilibiliCdn>().is_err());
    }

    #[test]
    fn backup_first_retains_original_and_limits_distinct_urls() {
        let primary = url("https://base.example/video");
        let first = url("https://first.example/video");
        let second = url("https://second.example/video");
        let unused = url("https://unused.example/video");
        let source = BilibiliCdn::BackupUrl
            .order(
                Some(primary.clone()),
                [
                    primary.clone(),
                    first.clone(),
                    first.clone(),
                    second.clone(),
                    unused,
                ]
                .into_iter(),
            )
            .unwrap();
        assert_eq!(source.credential_origin, primary.origin());
        assert_eq!(source.urls, [first, second, primary]);
    }

    #[test]
    fn first_backup_is_the_origin_when_the_primary_is_missing() {
        let first = url("https://first.example/video");
        let second = url("https://second.example/video");
        let source = BilibiliCdn::BackupUrl
            .order(None, [first.clone(), second.clone()].into_iter())
            .unwrap();
        assert_eq!(source.credential_origin, first.origin());
        assert_eq!(source.urls, [first, second]);
    }

    #[test]
    fn mirror_prefers_the_api_url_even_after_the_normal_fallback_slots() {
        let primary = url("https://base.example/video?upsig=primary");
        let first = url("https://first.example/video?upsig=first");
        let second = url("https://second.example/video?upsig=second");
        let exact_mirror = url("https://upos-sz-mirrorali.bilivideo.com/video?upsig=api");
        let source = "ali"
            .parse::<BilibiliCdn>()
            .unwrap()
            .order(
                Some(primary.clone()),
                [first.clone(), second, exact_mirror.clone()].into_iter(),
            )
            .unwrap();
        assert_eq!(source.credential_origin, primary.origin());
        assert_eq!(source.urls, [exact_mirror, primary, first]);
    }

    #[test]
    fn mirror_rewrite_keeps_the_original_path_and_query() {
        let primary = url("https://base.example:8443/a%2Fb?x=%2f%2F&upsig=a+b%2B");
        let backup = url("https://backup.example/video");
        let source = "akamai"
            .parse::<BilibiliCdn>()
            .unwrap()
            .order(Some(primary.clone()), [backup.clone()].into_iter())
            .unwrap();
        assert_eq!(source.credential_origin, primary.origin());
        assert_eq!(
            source.urls,
            [
                url("https://upos-hz-mirrorakam.akamaized.net/a%2Fb?x=%2f%2F&upsig=a+b%2B"),
                primary,
                backup,
            ]
        );
    }
}
