//! Google Places proxy (GOOGLE_MAPS_API_KEY). The key stays server-side; the
//! browser only sees our authenticated proxy.

use crate::{AppError, AppState, Config, resolve_auth};
use axum::{Json, extract::State, http::HeaderMap, response::IntoResponse, routing::get};

fn places_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| reqwest::Client::builder().build().unwrap())
}

fn places_key(config: &Config) -> Result<&str, AppError> {
    config
        .places_api_key
        .as_deref()
        .ok_or_else(|| AppError::bad_request("place autocomplete is not configured"))
}

#[derive(serde::Deserialize)]
struct AutocompleteQuery {
    q: String,
}

async fn places_autocomplete(
    State(AppState { pool, config, .. }): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<AutocompleteQuery>,
) -> Result<impl IntoResponse, AppError> {
    resolve_auth(&pool, &headers).await?;
    let key = places_key(&config)?;
    let resp = places_client()
        .post("https://places.googleapis.com/v1/places:autocomplete")
        .header("X-Goog-Api-Key", key)
        .header(
            "X-Goog-FieldMask",
            "suggestions.placePrediction.placeId,suggestions.placePrediction.text,suggestions.placePrediction.structuredFormat",
        )
        .json(&serde_json::json!({"input": q.q}))
        .send()
        .await
        .map_err(|e| AppError::bad_request(format!("places lookup failed: {e}")))?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    if !status.is_success() {
        return Err(AppError::bad_request(format!(
            "places lookup failed ({status})"
        )));
    }
    let items: Vec<serde_json::Value> = body["suggestions"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|s| {
            let p = &s["placePrediction"];
            let label = format!(
                "{} {}",
                p["structuredFormat"]["mainText"]["text"]
                    .as_str()
                    .unwrap_or(""),
                p["structuredFormat"]["secondaryText"]["text"]
                    .as_str()
                    .unwrap_or("")
            )
            .trim()
            .to_string();
            p["placeId"]
                .as_str()
                .map(|id| serde_json::json!({"label": label, "place_id": id}))
        })
        .collect();
    Ok(Json(serde_json::json!(items)))
}

/// First address component carrying `types` contains; short selects shortText.
fn address_component(details: &serde_json::Value, ty: &str, short: bool) -> Option<String> {
    details["addressComponents"]
        .as_array()?
        .iter()
        .find(|c| {
            c["types"]
                .as_array()
                .map(|ts| ts.iter().any(|t| t.as_str() == Some(ty)))
                .unwrap_or(false)
        })
        .and_then(|c| {
            c[if short { "shortText" } else { "longText" }]
                .as_str()
                .map(String::from)
        })
}

/// Maps a Places API (New) place details response onto the event
/// LocationBody shape, which create_location_from_body already stores.
fn place_details_to_location(details: &serde_json::Value) -> serde_json::Value {
    let street = match (
        address_component(details, "street_number", false),
        address_component(details, "route", false),
    ) {
        (Some(n), Some(r)) => Some(format!("{n} {r}")),
        (None, Some(r)) => Some(r),
        _ => None,
    };
    serde_json::json!({
        "provider": "google_places",
        "provider_place_id": details["id"].as_str(),
        "display_name": details["displayName"]["text"].as_str(),
        "formatted_address": details["formattedAddress"].as_str(),
        "street_address": street,
        "locality": address_component(details, "locality", false)
            .or_else(|| address_component(details, "sublocality", false)),
        "administrative_area": address_component(details, "administrative_area_level_1", true),
        "postal_code": address_component(details, "postal_code", false),
        "country": address_component(details, "country", true),
        "latitude": details["location"]["latitude"].as_f64(),
        "longitude": details["location"]["longitude"].as_f64(),
        "website": details["websiteUri"].as_str(),
        "phone": details["nationalPhoneNumber"].as_str(),
    })
}

async fn place_details(
    State(AppState { pool, config, .. }): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(place_id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, AppError> {
    resolve_auth(&pool, &headers).await?;
    let key = places_key(&config)?;
    let resp = places_client()
        .get(format!(
            "https://places.googleapis.com/v1/places/{place_id}?languageCode=en"
        ))
        .header("X-Goog-Api-Key", key)
        .header(
            "X-Goog-FieldMask",
            "id,displayName,formattedAddress,addressComponents,location,websiteUri,nationalPhoneNumber",
        )
        .send()
        .await
        .map_err(|e| AppError::bad_request(format!("places lookup failed: {e}")))?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    if !status.is_success() {
        return Err(AppError::bad_request(format!(
            "places lookup failed ({status})"
        )));
    }
    Ok(Json(place_details_to_location(&body)))
}

pub fn router() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route("/api/places/autocomplete", get(places_autocomplete))
        .route("/api/places/{place_id}", get(place_details))
}

#[cfg(test)]
mod places_tests {
    use super::{address_component, place_details_to_location};
    use serde_json::json;

    #[test]
    fn maps_place_details_to_location_body() {
        let details = json!({
            "id": "ChIJabc",
            "displayName": {"text": "Union Station"},
            "formattedAddress": "1701 Wynkoop St, Denver, CO 80202, USA",
            "addressComponents": [
                {"longText": "1701", "shortText": "1701", "types": ["street_number"]},
                {"longText": "Wynkoop Street", "shortText": "Wynkoop St", "types": ["route"]},
                {"longText": "Denver", "shortText": "Denver", "types": ["locality"]},
                {"longText": "Colorado", "shortText": "CO", "types": ["administrative_area_level_1"]},
                {"longText": "80202", "shortText": "80202", "types": ["postal_code"]},
                {"longText": "United States", "shortText": "US", "types": ["country"]}
            ],
            "location": {"latitude": 39.7534, "longitude": -105.0016},
            "websiteUri": "https://example.com",
            "nationalPhoneNumber": "(303) 555-0100"
        });
        let loc = place_details_to_location(&details);
        assert_eq!(loc["provider"], "google_places");
        assert_eq!(loc["provider_place_id"], "ChIJabc");
        assert_eq!(loc["display_name"], "Union Station");
        assert_eq!(loc["street_address"], "1701 Wynkoop Street");
        assert_eq!(loc["locality"], "Denver");
        assert_eq!(loc["administrative_area"], "CO");
        assert_eq!(loc["country"], "US");
        assert_eq!(loc["latitude"], 39.7534);
        assert_eq!(loc["website"], "https://example.com");
    }

    #[test]
    fn missing_components_stay_null() {
        let loc = place_details_to_location(&json!({"id": "x", "location": {}}));
        assert_eq!(loc["street_address"], serde_json::Value::Null);
        assert_eq!(loc["country"], serde_json::Value::Null);
    }

    #[test]
    fn sublocality_falls_back_for_locality() {
        let details = json!({"addressComponents": [
            {"longText": "Brooklyn", "shortText": "Brooklyn", "types": ["sublocality"]}
        ]});
        assert_eq!(
            address_component(&details, "sublocality", false),
            Some("Brooklyn".into())
        );
    }
}
