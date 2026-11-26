fn main() {
    println!("cargo:rerun-if-changed=src/airplay/playfair");
    cc::Build::new()
        .files([
            "src/airplay/playfair/playfair.c",
            "src/airplay/playfair/omg_hax.c",
            "src/airplay/playfair/hand_garble.c",
            "src/airplay/playfair/modified_md5.c",
            "src/airplay/playfair/sap_hash.c",
        ])
        .include("src/airplay/playfair")
        .warnings(false)
        .compile("playfair");
}
