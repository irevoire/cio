use async_trait::async_trait;
use barcoders::generators::image::*;
use barcoders::generators::svg::*;
use barcoders::sym::code39::*;
use google_drive::GoogleDrive;
use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, Stream};
use macros::db;
use reqwest::StatusCode;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::airtable::AIRTABLE_ASSET_ITEMS_TABLE;
use crate::companies::Company;
use crate::core::UpdateAirtableRecord;
use crate::db::Database;
use crate::schema::asset_items;
use crate::swag_inventory::image_to_pdf_object;

#[db {
    new_struct_name = "AssetItem",
    airtable_base = "assets",
    airtable_table = "AIRTABLE_ASSET_ITEMS_TABLE",
    match_on = {
        "cio_company_id" = "i32",
        "name" = "String",
    },
}]
#[derive(Debug, Insertable, AsChangeset, PartialEq, Clone, JsonSchema, Deserialize, Serialize)]
#[table_name = "asset_items"]
pub struct NewAssetItem {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty", deserialize_with = "airtable_api::attachment_format_as_string::deserialize")]
    pub picture: String,
    #[serde(default, skip_serializing_if = "String::is_empty", rename = "type")]
    pub type_: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub qualities: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub manufacturer: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model_number: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub serial_number: String,
    #[serde(default)]
    pub purchase_price: f32,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        serialize_with = "airtable_api::user_format_as_string::serialize",
        deserialize_with = "airtable_api::user_format_as_string::deserialize"
    )]
    pub current_employee_borrowing: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conference_room_using: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,

    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        serialize_with = "airtable_api::barcode_format_as_string::serialize",
        deserialize_with = "airtable_api::barcode_format_as_string::deserialize"
    )]
    pub barcode: String,
    #[serde(default, skip_serializing_if = "String::is_empty", deserialize_with = "airtable_api::attachment_format_as_string::deserialize")]
    pub barcode_png: String,
    #[serde(default, skip_serializing_if = "String::is_empty", deserialize_with = "airtable_api::attachment_format_as_string::deserialize")]
    pub barcode_svg: String,
    #[serde(default, skip_serializing_if = "String::is_empty", deserialize_with = "airtable_api::attachment_format_as_string::deserialize")]
    pub barcode_pdf_label: String,

    /// The CIO company ID.
    #[serde(default)]
    pub cio_company_id: i32,
}

/// Implement updating the Airtable record for a AssetItem.
#[async_trait]
impl UpdateAirtableRecord<AssetItem> for AssetItem {
    async fn update_airtable_record(&mut self, _record: AssetItem) {}
}

impl NewAssetItem {
    pub fn generate_barcode(&mut self) {
        let mut barcode = self
            .name
            .to_uppercase()
            .replace(' ', "")
            .replace('/', "")
            .replace('(', "")
            .replace(')', "")
            .replace('-', "")
            .replace("'", "")
            .trim()
            .to_string();

        // Add zeros to start of barcode til it is 39 chars long.
        // This makes sure the barcodes are all of uniform length.
        // To fit on the barcode label with the right DPI we CANNOT exceed this
        // legth.
        let max_barcode_len = 13;
        while barcode.len() < max_barcode_len {
            barcode = format!("0{}", barcode);
        }
        if barcode.len() > max_barcode_len {
            println!("len too long {} {}, needs to be {} or under", barcode, barcode.len(), max_barcode_len);
        }

        self.barcode = barcode;
    }

    pub async fn generate_barcode_images(&mut self, drive_client: &GoogleDrive, drive_id: &str, parent_id: &str) {
        // Generate the barcode.
        // "Name" is automatically generated by Airtable from the item and the size.
        if !self.name.is_empty() {
            // Generate the barcode svg and png.
            let barcode = Code39::new(&self.barcode).unwrap();
            let png = Image::png(45); // You must specify the height in pixels.
            let encoded = barcode.encode();

            // Image generators return a Result<Vec<u8>, barcoders::error::Error) of encoded bytes.
            let png_bytes = png.generate(&encoded[..]).unwrap();
            let mut file_name = format!("{} {}.png", self.type_, self.name.replace('/', ""));

            // Create or update the file in the google drive.
            let png_file = drive_client.create_or_update_file(drive_id, parent_id, &file_name, "image/png", &png_bytes).await.unwrap();
            self.barcode_png = format!("https://drive.google.com/uc?export=download&id={}", png_file.id);

            // Now do the SVG.
            let svg = SVG::new(200); // You must specify the height in pixels.
            let svg_data: String = svg.generate(&encoded).unwrap();
            let svg_bytes = svg_data.as_bytes();

            file_name = format!("{}, {}.svg", self.type_, self.name.replace('/', ""));

            // Create or update the file in the google drive.
            let svg_file = drive_client.create_or_update_file(drive_id, parent_id, &file_name, "image/svg+xml", &svg_bytes).await.unwrap();
            self.barcode_svg = format!("https://drive.google.com/uc?export=download&id={}", svg_file.id);

            // Generate the barcode label.
            let label_bytes = self.generate_pdf_barcode_label(&png_bytes);
            file_name = format!("{} {} - Barcode Label.pdf", self.type_, self.name.replace('/', ""));
            // Create or update the file in the google drive.
            let label_file = drive_client.create_or_update_file(drive_id, parent_id, &file_name, "application/pdf", &label_bytes).await.unwrap();
            self.barcode_pdf_label = format!("https://drive.google.com/uc?export=download&id={}", label_file.id);
        }
    }

    // Get the bytes for a pdf barcode label.
    pub fn generate_pdf_barcode_label(&self, png_bytes: &[u8]) -> Vec<u8> {
        let pdf_width = 3.0 * 72.0;
        let pdf_height = 2.0 * 72.0;
        let pdf_margin = 5.0;
        let font_size = 9.0;
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Courier",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! {
                "F1" => font_id,
            },
        });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), (font_size / 1.25).into()]),
                Operation::new("TL", vec![(font_size * 1.25).into()]),
                Operation::new("Td", vec![pdf_margin.into(), (font_size * 0.9 * 3.0).into()]),
                Operation::new("Tj", vec![Object::string_literal(self.barcode.to_string())]),
                Operation::new("Tf", vec!["F1".into(), font_size.into()]),
                Operation::new("'", vec![Object::string_literal(self.name.to_string())]),
                Operation::new("'", vec![Object::string_literal(format!("Type: {}", self.type_))]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
        });

        let pages = dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
            "Resources" => resources_id,
            // This should be (4 in x 6 in) for the rollo printer.
            // You get `pts` by (inches * 72).
            "MediaBox" => vec![0.into(), 0.into(),pdf_width.into(), pdf_height.into()],
        };
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        let logo_bytes = include_bytes!("oxide_logo.png");
        let (mut doc, logo_stream, mut logo_info) = image_to_pdf_object(doc, logo_bytes);
        // We want the logo width to fit.
        let original_width = logo_info.width;
        logo_info.width = pdf_width - (pdf_margin * 2.0);
        logo_info.height *= logo_info.width / original_width;
        let position = ((pdf_width - logo_info.width) / 2.0, pdf_height - logo_info.height - pdf_margin);
        // Center the logo at the top of the pdf.
        doc.insert_image(page_id, logo_stream, position, (logo_info.width, logo_info.height)).unwrap();

        let (mut doc, img_stream, info) = image_to_pdf_object(doc, png_bytes);
        // We want the barcode width to fit.
        // This will center it automatically.
        let position = ((pdf_width - info.width) / 2.0, pdf_height - info.height - logo_info.height - (pdf_margin * 2.0));
        // Center the barcode at the top of the pdf.
        doc.insert_image(page_id, img_stream, position, (info.width, info.height)).unwrap();

        doc.compress();

        // Save the PDF
        let mut buffer = Vec::new();
        doc.save_to(&mut buffer).unwrap();
        buffer
    }

    pub async fn expand(&mut self, drive_client: &GoogleDrive, drive_id: &str, parent_id: &str) {
        self.generate_barcode();
        self.generate_barcode_images(drive_client, drive_id, parent_id).await;
    }
}

/// A request to print labels.
#[derive(Debug, Clone, Default, JsonSchema, Deserialize, Serialize)]
pub struct PrintLabelsRequest {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default)]
    pub quantity: i32,
}

impl AssetItem {
    /// Send the label to our printer.
    pub async fn print_label(&self, db: &Database) {
        if self.barcode_pdf_label.trim().is_empty() {
            // Return early.
            return;
        }

        let company = self.company(db);

        if company.printer_url.is_empty() {
            // Return early.
            return;
        }

        let printer_url = format!("{}/zebra", company.printer_url);
        let client = reqwest::Client::new();
        let resp = client
            .post(&printer_url)
            .body(
                json!(PrintLabelsRequest {
                    url: self.barcode_pdf_label.to_string(),
                    quantity: 1,
                })
                .to_string(),
            )
            .send()
            .await
            .unwrap();
        match resp.status() {
            StatusCode::ACCEPTED => (),
            s => {
                panic!("[print]: status_code: {}, body: {}", s, resp.text().await.unwrap());
            }
        };
    }
}

/// Sync asset items from Airtable.
pub async fn refresh_asset_items(db: &Database, company: &Company) {
    // Get gsuite token.
    let token = company.authenticate_google(&db).await;

    // Initialize the Google Drive client.
    let drive_client = GoogleDrive::new(token);

    // Figure out where our directory is.
    // It should be in the shared drive : "Automated Documents"/"rfds"
    let shared_drive = drive_client.get_drive_by_name("Automated Documents").await.unwrap();
    let drive_id = shared_drive.id.to_string();

    // Get the directory by the name.
    let drive_assets_dir = drive_client.get_file_by_name(&drive_id, "assets").await.unwrap();
    let parent_id = drive_assets_dir.get(0).unwrap().id.to_string();

    // Get all the records from Airtable.
    let mut generator = names::Generator::default();
    let results: Vec<airtable_api::Record<AssetItem>> = company
        .authenticate_airtable(&company.airtable_base_id_assets)
        .list_records(&AssetItem::airtable_table(), "Grid view", vec![])
        .await
        .unwrap();
    for item_record in results {
        let mut item: NewAssetItem = item_record.fields.into();
        if item.name.is_empty() {
            item.name = generator.next().unwrap();
        }
        item.expand(&drive_client, &drive_id, &parent_id).await;
        item.cio_company_id = company.id;

        let mut db_item = item.upsert_in_db(&db);
        db_item.airtable_record_id = item_record.id.to_string();
        db_item.update(&db).await;
    }
}

#[cfg(test)]
mod tests {
    use crate::asset_inventory::{refresh_asset_items, AssetItems};
    use crate::companies::Company;
    use crate::db::Database;

    #[ignore]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_asset_items() {
        let db = Database::new();

        // Get the company id for Oxide.
        // TODO: split this out per company.
        let oxide = Company::get_from_db(&db, "Oxide".to_string()).unwrap();

        refresh_asset_items(&db, &oxide).await;
        AssetItems::get_from_db(&db, oxide.id).update_airtable(&db).await;
    }
}
