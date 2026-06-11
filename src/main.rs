use clap::Parser;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use rand::seq::SliceRandom;
use serde::Deserialize;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

/// votex-mailer — send unique poll links to a list of recipients.
///
/// Reads email addresses from EMAILS_FILE (one per line) and poll links from
/// LINKS_FILE (JSON).  Each recipient is assigned exactly one randomly chosen
/// link.  The number of email addresses must match the number of links.
///
/// Required environment variables:
///   GMAIL_USER      – your Gmail address (used as the sender)
///   GMAIL_PASSWORD  – your Gmail App Password
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Path to a plain-text file containing one email address per line
    #[arg(short, long, value_name = "FILE")]
    emails: PathBuf,

    /// Path to the JSON file containing the poll subject and links array
    #[arg(short, long, value_name = "FILE")]
    links: PathBuf,
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug, PartialEq)]
pub struct PollData {
    pub subject: Option<String>,
    pub links: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Pure helper functions (also used by tests)
// ---------------------------------------------------------------------------

/// Read all lines from a file, returning them as a `Vec<String>`.
pub fn read_lines<P>(filename: P) -> io::Result<Vec<String>>
where
    P: AsRef<Path>,
{
    let file = File::open(filename)?;
    BufReader::new(file).lines().collect()
}

/// Trim whitespace and remove empty strings from a `Vec<String>`.
pub fn clean_strings(raw: Vec<String>) -> Vec<String> {
    raw.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parse a JSON string that may contain multiple top-level objects (one per
/// line) and extract the `subject` and `links` fields.
pub fn parse_poll_json(json_content: &str) -> Result<(String, Vec<String>), String> {
    let mut subject = None;
    let mut links = None;

    let stream = serde_json::Deserializer::from_str(json_content).into_iter::<PollData>();
    for item in stream {
        let item = item.map_err(|e| format!("JSON parsing error: {}", e))?;
        if let Some(s) = item.subject {
            subject = Some(s);
        }
        if let Some(l) = item.links {
            links = Some(l);
        }
    }

    let subject = subject.ok_or_else(|| "Missing 'subject' key in JSON file".to_string())?;
    let links_raw = links.ok_or_else(|| "Missing 'links' key in JSON file".to_string())?;
    let links = clean_strings(links_raw);

    Ok((subject, links))
}

/// Verify that the email list and link list are non-empty and of equal length.
pub fn verify_counts(emails: &[String], links: &[String]) -> Result<(), String> {
    if emails.is_empty() {
        return Err("Verification failed: The email list is empty.".to_string());
    }
    if links.is_empty() {
        return Err("Verification failed: The links list is empty.".to_string());
    }
    if emails.len() != links.len() {
        return Err(format!(
            "Verification failed: Number of email addresses ({}) does not match \
             the number of links ({}) in the JSON file.",
            emails.len(),
            links.len()
        ));
    }
    Ok(())
}

/// Parse and validate all email strings into `lettre::message::Mailbox` values.
pub fn parse_mailboxes(emails: &[String]) -> Result<Vec<lettre::message::Mailbox>, String> {
    emails
        .iter()
        .map(|email| {
            email.parse::<lettre::message::Mailbox>().map_err(|e| {
                format!(
                    "Verification failed: Invalid recipient email syntax '{}': {}",
                    email, e
                )
            })
        })
        .collect()
}

/// Return a shuffled copy of `links` using the provided RNG.
pub fn shuffle_links<R: rand::Rng>(links: &[String], rng: &mut R) -> Vec<String> {
    let mut shuffled = links.to_vec();
    shuffled.shuffle(rng);
    shuffled
}

/// Derive the poll-results URL from a single vote link.
///
/// Vote link format:
///   http://votex.garypennington.net/vote?poll_id=<UUID>&token=<TOKEN>
/// Results URL format (scheme is preserved from the vote link):
///   http://votex.garypennington.net/poll/<UUID>/results
///
/// Returns `Err` if `poll_id` cannot be found in the query string.
pub fn results_url_from_vote_link(vote_link: &str) -> Result<String, String> {
    // Preserve the original scheme.
    let scheme = if vote_link.starts_with("https://") {
        "https"
    } else {
        "http"
    };

    // Locate the poll_id value in the query string.
    let poll_id = vote_link
        .split('?')
        .nth(1)
        .and_then(|qs| {
            qs.split('&').find_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                if k == "poll_id" { Some(v) } else { None }
            })
        })
        .ok_or_else(|| format!("Could not extract poll_id from vote link: {}", vote_link))?;

    // Extract the host from the vote link (strip scheme).
    let host = vote_link
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .ok_or_else(|| format!("Could not parse host from vote link: {}", vote_link))?;

    Ok(format!("{}://{}/poll/{}/results", scheme, host, poll_id))
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // 1. Load credentials from environment variables
    let gmail_user = std::env::var("GMAIL_USER")
        .map_err(|_| "GMAIL_USER environment variable matching your email is required")?;
    let gmail_password = std::env::var("GMAIL_PASSWORD")
        .map_err(|_| "GMAIL_PASSWORD environment variable (App Password) is required")?;

    // Validate sender address format up front
    let from_mailbox: lettre::message::Mailbox = gmail_user
        .parse()
        .map_err(|e| format!("Invalid GMAIL_USER email address syntax: {}", e))?;

    // 2. Validate that both files exist before doing anything else
    if !cli.emails.exists() {
        return Err(format!(
            "Emails file not found: '{}'\n  \
             Hint: supply a plain-text file with one email address per line \
             using the --emails flag.",
            cli.emails.display()
        )
        .into());
    }
    if !cli.links.exists() {
        return Err(format!(
            "Links JSON file not found: '{}'\n  \
             Hint: supply a JSON file containing a 'subject' string and a \
             'links' array using the --links flag.",
            cli.links.display()
        )
        .into());
    }

    println!("Loading emails from: {}", cli.emails.display());
    println!("Loading links JSON from: {}", cli.links.display());

    // 3. Read and clean email addresses
    let emails_raw = read_lines(&cli.emails).map_err(|e| {
        format!(
            "Failed to read emails file '{}': {}",
            cli.emails.display(),
            e
        )
    })?;
    let emails = clean_strings(emails_raw);

    // 4. Read and parse the links JSON
    let json_content = std::fs::read_to_string(&cli.links)
        .map_err(|e| format!("Failed to read JSON file '{}': {}", cli.links.display(), e))?;
    let (subject, links) = parse_poll_json(&json_content)
        .map_err(|e| format!("Failed to parse JSON file '{}': {}", cli.links.display(), e))?;

    // 5. Verify counts then validate every recipient address
    verify_counts(&emails, &links)?;
    let parsed_mailboxes = parse_mailboxes(&emails)?;

    println!("All verification steps passed successfully!");

    // 6. Derive the results URL once from the first link (all links share the same poll_id)
    let results_url = results_url_from_vote_link(&links[0])
        .map_err(|e| format!("Failed to derive results URL: {}", e))?;
    println!("Results URL: {}", results_url);

    // 7. Randomly assign one link per email address
    let shuffled_links = shuffle_links(&links, &mut rand::rng());

    // 8. Build the SMTP transport
    let creds = Credentials::new(gmail_user, gmail_password);
    let mailer = SmtpTransport::relay("smtp.gmail.com")?
        .credentials(creds)
        .build();

    // 8. Send
    println!(
        "Sending {} emails with subject '{}'...",
        emails.len(),
        subject
    );
    for (mailbox, link) in parsed_mailboxes.into_iter().zip(shuffled_links.iter()) {
        println!("Sending link to {}...", mailbox.email);

        let email_message = Message::builder()
            .from(from_mailbox.clone())
            .to(mailbox.clone())
            .subject(&subject)
            .body(format!(
                "Hello,\
                \n\nHere is your unique voting link:\n  {}\
                \n\nYou can view the results at:\n  {}\
                \n\nBest regards,",
                link, results_url
            ))?;

        match mailer.send(&email_message) {
            Ok(_) => println!("Successfully sent to {}!", mailbox.email),
            Err(e) => eprintln!("Failed to send to {}: {:?}", mailbox.email, e),
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::SmallRng;

    // -----------------------------------------------------------------------
    // clean_strings
    // -----------------------------------------------------------------------

    #[test]
    fn clean_strings_removes_blank_lines() {
        let input = vec![
            "  ".to_string(),
            "a@example.com".to_string(),
            "".to_string(),
            "b@example.com".to_string(),
        ];
        assert_eq!(clean_strings(input), vec!["a@example.com", "b@example.com"]);
    }

    #[test]
    fn clean_strings_trims_whitespace() {
        let input = vec![
            "  a@example.com  ".to_string(),
            "\tb@example.com\n".to_string(),
        ];
        assert_eq!(clean_strings(input), vec!["a@example.com", "b@example.com"]);
    }

    #[test]
    fn clean_strings_empty_input_returns_empty() {
        assert!(clean_strings(vec![]).is_empty());
    }

    // -----------------------------------------------------------------------
    // parse_poll_json
    // -----------------------------------------------------------------------

    #[test]
    fn parse_poll_json_single_object() {
        let json = r#"{"subject":"Vote!","links":["http://a.com","http://b.com"]}"#;
        let (subject, links) = parse_poll_json(json).unwrap();
        assert_eq!(subject, "Vote!");
        assert_eq!(links, vec!["http://a.com", "http://b.com"]);
    }

    #[test]
    fn parse_poll_json_multi_line_objects() {
        // Mirrors the actual on-disk format used by this application.
        let json = "{\"subject\":\"Which game?\"}\n{\"links\":[\"http://link1\",\"http://link2\"]}";
        let (subject, links) = parse_poll_json(json).unwrap();
        assert_eq!(subject, "Which game?");
        assert_eq!(links, vec!["http://link1", "http://link2"]);
    }

    #[test]
    fn parse_poll_json_missing_subject_returns_error() {
        let err = parse_poll_json(r#"{"links":["http://a.com"]}"#).unwrap_err();
        assert!(
            err.contains("Missing 'subject' key"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_poll_json_missing_links_returns_error() {
        let err = parse_poll_json(r#"{"subject":"Vote!"}"#).unwrap_err();
        assert!(
            err.contains("Missing 'links' key"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_poll_json_invalid_json_returns_error() {
        let err = parse_poll_json("not json at all").unwrap_err();
        assert!(
            err.contains("JSON parsing error"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_poll_json_filters_empty_links() {
        let json = r#"{"subject":"Vote!","links":["http://a.com","  ","","http://b.com"]}"#;
        let (_, links) = parse_poll_json(json).unwrap();
        assert_eq!(links, vec!["http://a.com", "http://b.com"]);
    }

    // -----------------------------------------------------------------------
    // verify_counts
    // -----------------------------------------------------------------------

    #[test]
    fn verify_counts_equal_lengths_ok() {
        let emails = vec!["a@x.com".to_string(), "b@x.com".to_string()];
        let links = vec!["http://l1".to_string(), "http://l2".to_string()];
        assert!(verify_counts(&emails, &links).is_ok());
    }

    #[test]
    fn verify_counts_empty_emails_returns_error() {
        let err = verify_counts(&[], &["http://l1".to_string()]).unwrap_err();
        assert!(
            err.contains("email list is empty"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn verify_counts_empty_links_returns_error() {
        let err = verify_counts(&["a@x.com".to_string()], &[]).unwrap_err();
        assert!(
            err.contains("links list is empty"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn verify_counts_mismatch_more_emails_than_links() {
        let emails = vec!["a@x.com".to_string(), "b@x.com".to_string()];
        let links = vec!["http://l1".to_string()];
        let err = verify_counts(&emails, &links).unwrap_err();
        assert!(err.contains("does not match"), "unexpected error: {err}");
        assert!(err.contains('2'), "should report email count: {err}");
        assert!(err.contains('1'), "should report link count: {err}");
    }

    #[test]
    fn verify_counts_mismatch_more_links_than_emails() {
        let emails = vec!["a@x.com".to_string()];
        let links = vec!["http://l1".to_string(), "http://l2".to_string()];
        let err = verify_counts(&emails, &links).unwrap_err();
        assert!(err.contains("does not match"), "unexpected error: {err}");
    }

    // -----------------------------------------------------------------------
    // parse_mailboxes
    // -----------------------------------------------------------------------

    #[test]
    fn parse_mailboxes_valid_addresses_ok() {
        let emails = vec![
            "alice@example.com".to_string(),
            "bob@example.com".to_string(),
        ];
        let mailboxes = parse_mailboxes(&emails).unwrap();
        assert_eq!(mailboxes.len(), 2);
        assert_eq!(mailboxes[0].email.to_string(), "alice@example.com");
        assert_eq!(mailboxes[1].email.to_string(), "bob@example.com");
    }

    #[test]
    fn parse_mailboxes_invalid_address_returns_error() {
        let emails = vec!["valid@example.com".to_string(), "notanemail".to_string()];
        let err = parse_mailboxes(&emails).unwrap_err();
        assert!(err.contains("notanemail"), "unexpected error: {err}");
        assert!(
            err.contains("Invalid recipient email syntax"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_mailboxes_empty_input_returns_empty_vec() {
        assert!(parse_mailboxes(&[]).unwrap().is_empty());
    }

    // -----------------------------------------------------------------------
    // results_url_from_vote_link
    // -----------------------------------------------------------------------

    const UUID: &str = "25369396-93bb-4a22-97b0-d38331186c6f";
    const TOKEN: &str = "9c3bec7000ac49a6a4099d8136af6dde";

    fn http_vote_link() -> String {
        format!(
            "http://votex.garypennington.net/vote?poll_id={}&token={}",
            UUID, TOKEN
        )
    }
    fn https_vote_link() -> String {
        format!(
            "https://votex.garypennington.net/vote?poll_id={}&token={}",
            UUID, TOKEN
        )
    }
    fn http_results_url() -> String {
        format!("http://votex.garypennington.net/poll/{}/results", UUID)
    }
    fn https_results_url() -> String {
        format!("https://votex.garypennington.net/poll/{}/results", UUID)
    }

    #[test]
    fn results_url_from_vote_link_http_stays_http() {
        assert_eq!(
            results_url_from_vote_link(&http_vote_link()).unwrap(),
            http_results_url()
        );
    }

    #[test]
    fn results_url_from_vote_link_https_stays_https() {
        assert_eq!(
            results_url_from_vote_link(&https_vote_link()).unwrap(),
            https_results_url()
        );
    }

    #[test]
    fn results_url_from_vote_link_token_first_in_query_string() {
        // poll_id is still found even when token comes first
        let link = format!(
            "http://votex.garypennington.net/vote?token={}&poll_id={}",
            TOKEN, UUID
        );
        assert_eq!(
            results_url_from_vote_link(&link).unwrap(),
            http_results_url()
        );
    }

    #[test]
    fn results_url_from_vote_link_missing_poll_id_returns_error() {
        let err = results_url_from_vote_link("http://votex.garypennington.net/vote?token=abc")
            .unwrap_err();
        assert!(err.contains("poll_id"), "unexpected error: {err}");
    }

    #[test]
    fn results_url_from_vote_link_no_query_string_returns_error() {
        let err = results_url_from_vote_link("http://votex.garypennington.net/vote").unwrap_err();
        assert!(err.contains("poll_id"), "unexpected error: {err}");
    }

    // -----------------------------------------------------------------------
    // shuffle_links
    // -----------------------------------------------------------------------

    #[test]
    fn shuffle_links_same_elements_different_order() {
        let links: Vec<String> = (1..=8).map(|i| format!("http://link{}", i)).collect();
        let mut rng = SmallRng::seed_from_u64(42);
        let shuffled = shuffle_links(&links, &mut rng);

        assert_eq!(shuffled.len(), links.len());
        let mut sorted_original = links.clone();
        let mut sorted_shuffled = shuffled.clone();
        sorted_original.sort();
        sorted_shuffled.sort();
        assert_eq!(sorted_original, sorted_shuffled);
    }

    #[test]
    fn shuffle_links_does_not_modify_original() {
        let links = vec!["http://a".to_string(), "http://b".to_string()];
        let original_clone = links.clone();
        let mut rng = SmallRng::seed_from_u64(0);
        let _ = shuffle_links(&links, &mut rng);
        assert_eq!(links, original_clone);
    }

    #[test]
    fn shuffle_links_single_element_unchanged() {
        let links = vec!["http://only".to_string()];
        let mut rng = SmallRng::seed_from_u64(99);
        assert_eq!(shuffle_links(&links, &mut rng), links);
    }

    #[test]
    fn shuffle_links_empty_input_returns_empty() {
        let mut rng = SmallRng::seed_from_u64(0);
        assert!(shuffle_links(&[], &mut rng).is_empty());
    }

    #[test]
    fn shuffle_links_different_seeds_may_produce_different_orders() {
        let links: Vec<String> = (1..=10).map(|i| format!("http://link{}", i)).collect();
        let shuffled_a = shuffle_links(&links, &mut SmallRng::seed_from_u64(1));
        let shuffled_b = shuffle_links(&links, &mut SmallRng::seed_from_u64(2));
        // Two different seeds should almost certainly yield different orderings
        // for a 10-element list (1/10! collision probability).
        assert_ne!(shuffled_a, shuffled_b);
    }
}
