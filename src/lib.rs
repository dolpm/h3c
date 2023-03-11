pub mod client;

#[cfg(test)]

mod tests {
    use crate::client::Client;
    use std::{str::FromStr, time::Instant};

    #[test]
    fn get() {
        let mut start = Instant::now();
        let mut c = Client::connect("ssl.gstatic.com").expect("failed to ocnnect to server");
        let resp_bytes = c
            .send(
                url::Url::from_str(
                    "https://ssl.gstatic.com/gb/images/sprites/p_2x_8b2dd64bfdb2.png",
                )
                .unwrap(),
                "GET",
                None,
                None,
            )
            .unwrap();

        println!("Done: {:?}", start.elapsed());

        start = Instant::now();

        let comp = reqwest::blocking::get(
            "https://ssl.gstatic.com/gb/images/sprites/p_2x_8b2dd64bfdb2.png",
        )
        .expect("failed to fetch")
        .bytes()
        .expect("failed to convert to bytes");

        println!("Done: {:?}", start.elapsed());

        assert_eq!(resp_bytes, comp);
    }
}
