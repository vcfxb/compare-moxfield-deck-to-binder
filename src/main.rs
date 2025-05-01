use dialoguer::Input;
use regex::Regex;
use reqwest::blocking::Response;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    env, process,
    sync::Once,
    time::{Duration, Instant},
};
use url::Url;

fn prompt_exit(status: i32) {
    Input::<String>::new()
        .allow_empty(true)
        .with_prompt("Press enter to exit")
        .interact()
        .unwrap();

    process::exit(status);
}

fn check_moxfield_url(s: &str) -> Result<Url, &'static str> {
    let Ok(url) = Url::parse(s) else {
        return Err("invalid url");
    };

    if url.domain() != Some("moxfield.com") {
        return Err("url is not a moxfield url");
    }

    Ok(url)
}

fn main() {
    dotenv::dotenv().ok();

    let user_agent = env::var("MOXFIELD_UA").unwrap_or_else(|_| {
        Input::new()
            .with_prompt(
                "Moxfield User Agent (you may have to email support@moxfield.com for yours)",
            )
            .interact_text()
            .expect("failed to read moxfield user agent")
    });

    let deck_url_string = Input::new()
        .with_prompt("Paste moxfield deck URL and hit enter")
        .validate_with(|input: &String| {
            let url = check_moxfield_url(input)?;

            if !url.path().starts_with("/decks/") {
                return Err("url does not relate to a deck");
            }

            Ok(())
        })
        .interact_text()
        .expect("failed to read moxfield url");

    // Validated above.
    let deck_url = Url::parse(&deck_url_string).unwrap();
    let deck_id = deck_url.path_segments().unwrap().nth(1).unwrap();

    // Download deck...
    println!("Downloading deck list for {deck_url} (deck_id: {deck_id})");

    let client = reqwest::blocking::ClientBuilder::new()
        .user_agent(user_agent)
        .build()
        .unwrap();

    let deck_request = client
        .get(format!(
            "https://api2.moxfield.com/v2/decks/all/{deck_id}/export"
        ))
        .header("Accept", "application/json, text/plain, */*")
        .query(&[
            ("arenaOnly", "false"),
            ("format", "full"),
            // As far as I can tell this UUIDv4 is a magic number -- it shows up in every test export request I make
            // via the website. Without it stuff seems to 404
            ("exportId", "35bf1ed6-a25c-4440-a0d3-dfba067c8582"),
        ]);

    let deck_response = match deck_request.send() {
        Ok(response) => response
            .text()
            .expect("moxfield responded with non-text data"),

        Err(e) => {
            println!("Could not download deck list from moxfield: {e}");
            dbg!(e);
            prompt_exit(1);
            unreachable!();
        }
    };

    // We need to wait a full secound before making another api request to avoid running afoul of ratelimits.
    let deck_response_time = Instant::now();

    let deck_format_line_regex =
        Regex::new(r#"(?<count>\d+)\s+(?<name>[^\(]+)\s+\((?<set>[A-Z0-9]+)\)\s+(?<cn>\d+).*"#)
            .unwrap();

    let mut deck = HashMap::new();
    let mut deck_card_count = 0;

    for line in deck_response.lines() {
        match deck_format_line_regex.captures(line) {
            None => {
                println!("Line failed to match deck regex: {line}");
                prompt_exit(1);
                unreachable!()
            }

            Some(captures) => {
                let entry: &mut i32 = deck
                    .entry(captures.name("name").unwrap().as_str())
                    .or_insert(0);

                let count = captures
                    .name("count")
                    .unwrap()
                    .as_str()
                    .parse::<i32>()
                    .unwrap();

                *entry += count;
                deck_card_count += count;
            }
        }
    }

    println!("Deck imported, {deck_card_count} cards.");

    // Deck import done, now go get binder.
    let binder_url_str = Input::new()
        .with_prompt("Moxfield binder url")
        .validate_with(|input: &String| {
            let url = check_moxfield_url(input)?;

            if !url.path().starts_with("/binders/") {
                return Err("url does not relate to a binder");
            }

            Ok(())
        })
        .interact_text()
        .expect("read moxfield binder url");

    let binder_url = Url::parse(&binder_url_str).unwrap();
    let binder_id = binder_url.path_segments().unwrap().nth(1).unwrap();

    let binder_query = client
        .get(format!(
            "https://api2.moxfield.com/v1/trade-binders/{binder_id}/search"
        ))
        .header("Accept", "application/json, text/plain, */*")
        .query(&json!({
            "pageNumber": 1,
            "pageSize": deck_card_count
        }));

    static PRINT_ONCE: Once = Once::new();
    while deck_response_time.elapsed() <= Duration::from_secs(1) {
        PRINT_ONCE.call_once(|| {
            println!("Waiting for moxfield ratelimit...");
        });
    }

    let binder_response = binder_query
        .send()
        .and_then(Response::json::<Value>)
        .unwrap_or_else(|e| {
            println!("Moxfield returned binder data we couldn't recognize: {e}");
            prompt_exit(1);
            unreachable!()
        });

    if binder_response["totalOverall"].as_i64().unwrap() > deck_card_count as i64 {
        println!(
            "Warning: Binder has more cards than deck, so the following data is likely inaccurate"
        );
    }

    let mut binder: HashMap<&str, i64> = HashMap::new();
    let mut recieved_binder_card_count = 0;

    for item in binder_response["data"].as_array().unwrap() {
        let quantity = item["quantity"].as_i64().unwrap();
        let name = item["card"]["name"].as_str().unwrap();

        *binder.entry(name).or_default() += quantity;
        recieved_binder_card_count += quantity;
    }

    if recieved_binder_card_count != binder_response["totalOverall"].as_i64().unwrap() {
        println!("Error: Did not recieve full binder.");
        prompt_exit(1);
        unreachable!()
    }

    println!("All data recieved. Comparing deck to binder...");

    let mut cards_in_common = 0;

    for card in deck.keys().chain(binder.keys()).collect::<HashSet<_>>() {
        match (
            deck.get(card).copied().unwrap_or_default() as i64,
            binder.get(card).copied().unwrap_or_default(),
        ) {
            (x, y) if x > y => println!("Deck contains {} copies of {card} not in binder", x - y),
            (x, y) if x < y => println!("Binder contains {} copies of {card} not in deck", y - x),
            (x, y) if x == y => cards_in_common += x,
            _ => unreachable!(),
        }
    }

    println!("Done comparing. Deck and binder have {cards_in_common} cards in common.");
    prompt_exit(0);
}
