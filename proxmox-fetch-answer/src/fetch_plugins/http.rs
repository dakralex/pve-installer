use anyhow::{bail, Error, Result};
use log::info;
use std::{
    fs::{self, read_to_string},
    process::Command,
};

use crate::fetch_plugins::utils::post;
use proxmox_auto_installer::{sysinfo, utils::AutoInstSettings};

static ANSWER_SUBDOMAIN: &str = "proxmoxinst";
static ANSWER_SUBDOMAIN_FP: &str = "proxmoxinst-fp";

// It is possible to set custom DHPC options. Option numbers 224 to 254 [0].
// To use them with dhclient, we need to configure it to request them and what they should be
// called.
//
// e.g. /etc/dhcp/dhclient.conf:
// ```
// option proxmoxinst-url code 250 = text;
// option proxmoxinst-fp code 251 = text;
// also request proxmoxinst-url, proxmoxinst-fp;
// ```
//
// The results will end up in the /var/lib/dhcp/dhclient.leases file from where we can fetch them
//
// [0] https://www.iana.org/assignments/bootp-dhcp-parameters/bootp-dhcp-parameters.xhtml
static DHCP_URL_OPTION: &str = "proxmoxinst-url";
static DHCP_FP_OPTION: &str = "proxmoxinst-fp";
static DHCP_LEASE_FILE: &str = "/var/lib/dhcp/dhclient.leases";

pub struct FetchFromHTTP;

impl FetchFromHTTP {
    /// Will try to fetch the answer.toml by sending a HTTP POST request. The URL can be configured
    /// either via DHCP or DNS or preconfigured in the ISO.
    /// If the URL is not defined in the ISO, it will first check DHCP options. The SSL certificate
    /// needs to be either trusted by the root certs or a SHA256 fingerprint needs to be provided.
    /// The SHA256 SSL fingerprint can either be defined in the ISO, as DHCP option, or as DNS TXT
    /// record. If provided, the fingerprint provided in the ISO has preference.
    pub fn get_answer(settings: &AutoInstSettings) -> Result<String> {
        let mut fingerprint: Option<String> = match settings.cert_fingerprint.clone() {
            Some(fp) => {
                info!("SSL fingerprint provided through ISO.");
                Some(fp)
            }
            None => None,
        };

        let answer_url: String;
        if let Some(url) = settings.http_url.clone() {
            info!("URL specified in ISO");
            answer_url = url;
        } else {
            (answer_url, fingerprint) = match Self::fetch_dhcp(fingerprint.clone()) {
                Ok((url, fp)) => (url, fp),
                Err(err) => {
                    info!("{err}");
                    Self::fetch_dns(fingerprint.clone())?
                }
            };
        }

        if fingerprint.is_some() {
            let fp = fingerprint.clone();
            fs::write("/tmp/cert_fingerprint", fp.unwrap()).ok();
        }

        info!("Gathering system information.");
        let payload = sysinfo::get_sysinfo(false)?;
        info!("Sending POST request to '{answer_url}'.");
        let answer = post::call(answer_url, fingerprint.as_deref(), payload)?;
        Ok(answer)
    }

    /// Fetches search domain from resolv.conf file
    fn get_search_domain() -> Result<String> {
        info!("Retrieving default search domain.");
        for line in read_to_string("/etc/resolv.conf")?.lines() {
            if let Some((key, value)) = line.split_once(' ') {
                if key == "search" {
                    return Ok(value.trim().into());
                }
            }
        }
        Err(Error::msg("Could not find search domain in resolv.conf."))
    }

    /// Runs a TXT DNS query on the domain provided
    fn query_txt_record(query: String) -> Result<String> {
        info!("Querying TXT record for '{query}'");
        let url: String;
        match Command::new("dig")
            .args(["txt", "+short"])
            .arg(&query)
            .output()
        {
            Ok(output) => {
                if output.status.success() {
                    url = String::from_utf8(output.stdout)?
                        .replace('"', "")
                        .trim()
                        .into();
                    if url.is_empty() {
                        bail!("Got empty response.");
                    }
                } else {
                    bail!(
                        "Error querying DNS record '{query}' : {}",
                        String::from_utf8(output.stderr)?
                    );
                }
            }
            Err(err) => bail!("Error querying DNS record '{query}': {err}"),
        }
        info!("Found: '{url}'");
        Ok(url)
    }

    /// Tries to fetch answer URL and SSL fingerprint info from DNS
    fn fetch_dns(mut fingerprint: Option<String>) -> Result<(String, Option<String>)> {
        let search_domain = Self::get_search_domain()?;

        let answer_url = match Self::query_txt_record(format!("{ANSWER_SUBDOMAIN}.{search_domain}"))
        {
            Ok(url) => url,
            Err(err) => bail!("{err}"),
        };

        if fingerprint.is_none() {
            fingerprint =
                match Self::query_txt_record(format!("{ANSWER_SUBDOMAIN_FP}.{search_domain}")) {
                    Ok(fp) => Some(fp),
                    Err(err) => {
                        info!("{err}");
                        None
                    }
                };
        }
        Ok((answer_url, fingerprint))
    }

    /// Tries to fetch answer URL and SSL fingerprint info from DHCP options
    fn fetch_dhcp(mut fingerprint: Option<String>) -> Result<(String, Option<String>)> {
        info!("Checking DHCP options.");
        let leases = fs::read_to_string(DHCP_LEASE_FILE)?;

        let mut answer_url: Option<String> = None;

        let url_match = format!("option {DHCP_URL_OPTION}");
        let fp_match = format!("option {DHCP_FP_OPTION}");

        for line in leases.lines() {
            if answer_url.is_none() && line.trim().starts_with(url_match.as_str()) {
                answer_url = Self::strip_dhcp_option(line.split(' ').nth_back(0));
            }
            if fingerprint.is_none() && line.trim().starts_with(fp_match.as_str()) {
                fingerprint = Self::strip_dhcp_option(line.split(' ').nth_back(0));
            }
        }

        let answer_url = match answer_url {
            None => bail!("No DHCP option found for fetch URL."),
            Some(url) => {
                info!("Found URL for answer in DHCP option: '{url}'");
                url
            }
        };

        if let Some(fp) = fingerprint.clone() {
            info!("Found SSL Fingerprint via DHCP: '{fp}'");
        }

        Ok((answer_url, fingerprint))
    }

    /// Clean DHCP option string
    fn strip_dhcp_option(value: Option<&str>) -> Option<String> {
        // value is expected to be in format: "value";
        value.map(|value| String::from(&value[1..value.len() - 2]))
    }
}