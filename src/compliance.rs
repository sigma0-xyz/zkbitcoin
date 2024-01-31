use anyhow::{Context, Result};
use chrono::prelude::*;
use fancy_regex::Regex;
use futures::StreamExt;
use log::{error, info};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::{spawn, task::JoinHandle, time::interval};
use xml::reader::{EventReader, XmlEvent};

pub struct AddressVerifier {
    sanctioned_addresses: HashMap<String, bool>,
    last_update: i64,
}

impl AddressVerifier {
    const BTC_ID: &'static str = "344";
    const OFAC_URL: &'static str =
        "https://www.treasury.gov/ofac/downloads/sanctions/1.0/sdn_advanced.xml";

    pub fn new() -> Self {
        Self {
            sanctioned_addresses: HashMap::new(),
            last_update: 0,
        }
    }

    fn extract_from_xml(str_value: &str, tag: &str) -> Result<u32> {
        let re = Regex::new(&format!(r"(?<={}>)\s*(\w+)(?=<\/{})", tag, tag)).unwrap();
        let value = re.find(&str_value)?.context("no regex result")?.as_str();

        Ok(value.parse()?)
    }

    /// read the first few bytes from the remote XML file and extract the last update date.
    /// If there is no fresh data we can skip the parsing of XML which is slow.
    async fn publish_date() -> Result<i64> {
        let res = reqwest::get(Self::OFAC_URL).await?;

        let head = res
            .bytes_stream()
            .take(1)
            .collect::<Vec<reqwest::Result<_>>>()
            .await
            .into_iter()
            .collect::<reqwest::Result<Vec<_>>>()?;

        let str_value = String::from_utf8(head[0].to_vec())?;
        let year = Self::extract_from_xml(&str_value, "Year")?;
        let day = Self::extract_from_xml(&str_value, "Day")?;
        let month = Self::extract_from_xml(&str_value, "Month")?;
        let date = Utc
            .with_ymd_and_hms(year as i32, month, day, 0, 0, 0)
            .single()
            .context("date parse error")?
            .timestamp();

        Ok(date)
    }

    /// Runs the Sanction list syncronization. Downloads the remote XML file and extracts the sanctioned addresses
    pub async fn sync(&mut self) -> Result<()> {
        let publish_date = Self::publish_date().await?;
        if self.last_update >= publish_date {
            info!("Sanction list is up-to-date");
            return Ok(());
        }

        self.last_update = publish_date;

        info!("Syncing sanction list...");
        let start = Instant::now();
        let res = reqwest::get(Self::OFAC_URL).await?;

        let xml = res.text().await?;
        let parser: EventReader<&[u8]> = EventReader::new(xml.as_bytes());
        let mut inside_feature_elem = false;
        let mut inside_final_elem = false;

        for e in parser {
            match e {
                Ok(XmlEvent::StartElement {
                    name, attributes, ..
                }) => {
                    if name.local_name == "Feature" {
                        if attributes.iter().any(|a| {
                            a.name.local_name == "FeatureTypeID" && a.value == Self::BTC_ID
                        }) {
                            inside_feature_elem = true;
                        }
                    } else if name.local_name == "VersionDetail" && inside_feature_elem {
                        inside_final_elem = true;
                    }
                }
                Ok(XmlEvent::Characters(value)) => {
                    if inside_final_elem {
                        self.sanctioned_addresses.insert(value, true);
                    }
                }
                Ok(XmlEvent::EndElement { name, .. }) => {
                    if name.local_name == "VersionDetail" && inside_feature_elem {
                        inside_feature_elem = false;
                        inside_final_elem = false;
                    }
                }
                Err(e) => {
                    error!("Error parsing xml: {e}");
                    break;
                }
                _ => {}
            }
        }

        let duration = start.elapsed();
        info!("Sanction list synced in {:?}", duration);

        Ok(())
    }

    /// Periodically fetces the latest list from OFAC_URL and updates the local list
    pub fn start(&'static mut self) -> JoinHandle<()> {
        spawn(async move {
            let mut interval = interval(Duration::from_secs(600));

            loop {
                interval.tick().await;

                if let Err(error) = self.sync().await {
                    error!("Sanction list sync error: {}", error);
                };
            }
        })
    }

    /// Returns true if the given address is in the sanction list
    pub async fn is_sanctioned(&self, address: &str) -> bool {
        self.sanctioned_addresses.contains_key(address)
    }
}