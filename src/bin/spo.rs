use convert_invert::internals::utils::config::config_manager::Config;
use spotify_rs::{ClientCredsClient, model::PlayableItem};

#[tokio::main]
async fn main() {
    let config = Config::try_from_env().unwrap();
    let spotify =
        ClientCredsClient::authenticate(config.client_id.unwrap(), config.client_secret.unwrap())
            .await
            .unwrap();
    let pleta = spotify_rs::playlist("1B3Q6EB9Pjb57jKywHJPfq?si=86e66686d3ef47c4")
        .market("US")
        .get(&spotify)
        .await
        .unwrap();
    while let Ok(page) = pleta.tracks.get_next(&spotify).await {
        page.items.into_iter().for_each(|track| {
            let track = track.unwrap().track;
            if let PlayableItem::Track(song) = track {
                println!("{} - {}", song.artists.first().unwrap().name, song.name)
            }
        });
    }
}
