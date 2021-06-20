use crate::{db, doh, Domain, DomainSetShardedFX};
use std::str::FromStr;
use std::sync::Arc;
use tokio_stream::StreamExt;

const DNS_RECORD_DIR: &str = "dns_db";

#[derive(Clone, Debug)]
pub struct DNSResultRecord {
    pub domain: Domain,
    pub cnames: Vec<Domain>,
    pub ips: Vec<std::net::IpAddr>,
}

impl FromStr for DNSResultRecord {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split(';');
        let domain: Domain = parts.next().ok_or(())?.parse().map_err(|_| ())?;
        let mut cnames: Vec<Domain> = Vec::new();
        for cname in parts.next().ok_or(())?.split(',').filter(|c| !c.is_empty()) {
            cnames.push(cname.parse().map_err(|_| ())?)
        }
        let mut ips: Vec<std::net::IpAddr> = Vec::new();
        for ip in parts
            .next()
            .ok_or(())?
            .trim_end()
            .split(',')
            .filter(|c| !c.is_empty())
        {
            ips.push(ip.parse().map_err(|_| ())?)
        }
        Ok(DNSResultRecord {
            domain,
            cnames,
            ips,
        })
    }
}

impl std::fmt::Display for DNSResultRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let mut output = String::new();
        output.push_str(&self.domain);
        output.push(';');
        for cname in self.cnames.iter() {
            output.push_str(&cname);
            output.push(',');
        }
        output.push(';');
        for ip in self.ips.iter() {
            output.push_str(&ip.to_string());
            output.push(',');
        }
        write!(f, "{}", output)
    }
}

pub trait DomainRecordHandler: Send + Sync {
    fn handle_domain_record(&self, record: &DNSResultRecord);
}

#[derive(Clone)]
struct DNSDBReader<T: DomainRecordHandler> {
    domains: Arc<DomainSetShardedFX>,
    record_handler: Arc<T>,
}

impl<T: DomainRecordHandler> DNSDBReader<T> {
    fn new(record_handler: Arc<T>, domains: Arc<DomainSetShardedFX>) -> Self {
        DNSDBReader {
            domains,
            record_handler,
        }
    }
}

impl<T: DomainRecordHandler> db::DBReadHandler for DNSDBReader<T> {
    fn handle_input(&self, data: &str) {
        if let Ok(record) = data.parse::<DNSResultRecord>() {
            self.domains.remove_str(&record.domain);
            self.record_handler.handle_domain_record(&record)
        }
    }
    fn finished_with_file(&self) {
        self.domains.shrink_to_fit();
    }
}

async fn get_dns_results<T: 'static + DomainRecordHandler>(
    dns_record_handler: Arc<T>,
    client: reqwest::Client,
    server: Arc<String>,
    domain: Domain,
) -> Option<DNSResultRecord> {
    tokio::spawn(async move {
        let result = doh::lookup_domain(server, client, 1, &domain).await.ok()?;
        if let Some(record) = &result {
            dns_record_handler.handle_domain_record(&record);
        }
        Some(result.unwrap_or_else(|| DNSResultRecord {
            domain,
            cnames: Vec::new(),
            ips: Vec::new(),
        }))
    })
    .await
    .ok()?
}

pub async fn lookup_domains<T: 'static + DomainRecordHandler>(
    domains: DomainSetShardedFX,
    dns_record_handler: Arc<T>,
    servers: &[Arc<String>],
    client: &reqwest::Client,
    concurrent_requests: usize,
    dns_max_age: u64,
    file_max_size: usize,
) -> Result<(), std::io::Error> {
    let domains_arc = Arc::new(domains);
    let db_record_handler = DNSDBReader::new(dns_record_handler.clone(), domains_arc.clone());

    db::dir_db_read(
        Arc::new(db_record_handler),
        &std::path::Path::new(DNS_RECORD_DIR),
        dns_max_age,
    )
    .await?;

    let domains = Arc::try_unwrap(domains_arc)
        .ok()
        .expect("Failed to unwrap Arc");
    println!("Looking up {} domains", domains.len());
    if domains.is_empty() {
        return Ok(());
    }
    let total_length = domains.len();
    let mut domain_iter = domains.into_iter_domains();
    let mut tasks: futures::stream::FuturesUnordered<_> = (&mut domain_iter)
        .take(concurrent_requests)
        .enumerate()
        .map(|(i, domain)| {
            get_dns_results(
                dns_record_handler.clone(),
                client.clone(),
                servers[i % servers.len()].clone(),
                domain,
            )
        })
        .collect();
    println!("Created initial tasks");
    let now = std::time::Instant::now();
    let mut i: usize = 0;
    let mut error_count: usize = 0;
    let display_status = |i: usize, error_count: usize, now: &std::time::Instant| {
        println!(
            "{}/{} {}/s with {} errors",
            i,
            total_length,
            i as f32 / now.elapsed().as_secs_f32(),
            error_count,
        )
    };

    let mut wtr =
        db::DirDbWriter::new(&std::path::Path::new(DNS_RECORD_DIR), file_max_size, None).await?;

    let mut since_last_output = std::time::Instant::now();
    while let Some(record) = tasks.next().await {
        if let Some(record) = record {
            if since_last_output.elapsed().as_secs() > 1 {
                display_status(i, error_count, &now);
                since_last_output = std::time::Instant::now();
            }
            wtr.write_line(record.to_string().as_bytes()).await?;
        } else {
            error_count += 1;
        }
        if let Some(next_domain) = domain_iter.next() {
            tasks.push(get_dns_results(
                dns_record_handler.clone(),
                client.clone(),
                servers[i % servers.len()].clone(),
                next_domain,
            ));
        }
        i += 1;
    }
    wtr.flush().await?;
    display_status(i, error_count, &now);
    Ok(())
}
