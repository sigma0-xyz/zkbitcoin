use anyhow::{Context, Result};
use chrono::prelude::*;
use fancy_regex::Regex;
use futures::StreamExt;
use log::{error, info};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{spawn, sync::RwLock, time::interval};
use xml::reader::{EventReader, XmlEvent};

pub struct AddressVerifier {
    sanctioned_addresses: Arc<RwLock<HashMap<String, bool>>>,
    last_update: Arc<RwLock<i64>>,
}

impl AddressVerifier {
    const BTC_ID: &'static str = "344";

    pub fn new() -> Self {
        Self {
            sanctioned_addresses: Arc::new(RwLock::new(HashMap::new())),
            last_update: Arc::new(RwLock::new(0)),
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
        let res =
            reqwest::get("https://www.treasury.gov/ofac/downloads/sanctions/1.0/sdn_advanced.xml")
                .await?;

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

    /// Periodically fetces the latest list from https://www.treasury.gov/ofac/downloads/sanctions/1.0/sdn_advanced.xml
    /// and updates the list
    pub async fn start(&self) {
        let sanctioned_addresses = Arc::clone(&self.sanctioned_addresses);
        let last_update = Arc::clone(&self.last_update);

        spawn(async move {
            let mut interval = interval(Duration::from_secs(600));

            loop {
                let Ok(publish_date) = Self::publish_date().await else {
                    error!("couldn't extract the OFAC document publish date");
                    continue;
                };

                if *last_update.read().await >= publish_date {
                    info!("OFAC list is up-to-date");
                    continue;
                }

                let mut sanctioned_addresses = sanctioned_addresses.write().await;
                interval.tick().await;

                let mut last_update = last_update.write().await;
                *last_update = publish_date;

                info!("OFAC list syncing...");
                let start = Instant::now();
                let Ok(res) = reqwest::get(
                    "https://www.treasury.gov/ofac/downloads/sanctions/1.0/sdn_advanced.xml",
                )
                .await
                else {
                    error!("couldn't fetch OFAC list");
                    continue;
                };
                let Ok(xml) = res.text().await else {
                    error!("couldn't parse OFAC list");
                    continue;
                };
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
                                sanctioned_addresses.insert(value, true);
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
                info!("OFAC synced in {:?}", duration);
            }
        })
        .await
        .unwrap();
    }
}