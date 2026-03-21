use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const ADJECTIVES: &[&str] = &[
    "amber", "ancient", "azure", "blazing", "bold", "brave", "bright", "broad", "calm", "carved",
    "clear", "clever", "cold", "cool", "coral", "crisp", "crystal", "dark", "deep", "deft", "dry",
    "eager", "endless", "fair", "fallen", "fast", "fierce", "fine", "firm", "fleet", "flowing",
    "flying", "foggy", "free", "frozen", "gentle", "gilded", "glad", "glass", "gold", "grand",
    "green", "grey", "hidden", "hollow", "humble", "hushed", "iron", "ivory", "keen", "kind",
    "last", "late", "lean", "light", "little", "lone", "long", "lost", "lucky", "lucid", "mild",
    "misty", "mossy", "muted", "narrow", "neat", "noble", "north", "odd", "open", "outer", "pale",
    "plain", "proud", "pure", "quiet", "quick", "rare", "raw", "red", "risen", "rough", "round",
    "rugged", "rustic", "safe", "salty", "scarlet", "secret", "serene", "sharp", "sheer", "silent",
    "silver", "sleek", "slim", "slow", "small", "smooth", "soft", "solid", "south", "spare",
    "stark", "steady", "steep", "still", "stony", "strong", "subtle", "sunlit", "sure", "sweet",
    "swift", "tall", "tidal", "tidy", "torn", "tough", "true", "twin", "upper", "vast", "vivid",
    "warm", "waxen", "west", "white", "wide", "wild", "wise", "woven", "young",
];

const NOUNS: &[&str] = &[
    "anchor", "arch", "ash", "aurora", "basin", "bay", "beacon", "beam", "bell", "birch", "blade",
    "bloom", "bluff", "branch", "breeze", "bridge", "brook", "cairn", "canyon", "cape", "cedar",
    "chalk", "cliff", "cloud", "coast", "coral", "cove", "crane", "creek", "crest", "crown",
    "crystal", "dale", "dawn", "delta", "dew", "dove", "drift", "dune", "dusk", "eagle", "echo",
    "edge", "elm", "ember", "falcon", "feather", "fern", "field", "fjord", "flame", "flint",
    "forge", "fox", "frost", "garden", "gate", "glade", "glen", "gorge", "grove", "harbor",
    "haven", "hawk", "hazel", "heath", "heron", "hill", "hollow", "horizon", "inlet", "iron",
    "isle", "ivy", "jade", "lantern", "larch", "lark", "laurel", "leaf", "ledge", "linden",
    "lodge", "marsh", "meadow", "mesa", "mist", "moon", "moss", "oak", "oasis", "ocean", "orbit",
    "otter", "owl", "pass", "path", "peak", "pebble", "pine", "plateau", "plume", "pond",
    "prairie", "quarry", "rain", "rapids", "raven", "reef", "ridge", "river", "robin", "sage",
    "seal", "shore", "sierra", "silver", "sky", "slate", "snow", "spark", "spring", "star",
    "stone", "storm", "stream", "summit", "sun", "temple", "thistle", "thorn", "thunder", "tide",
    "tower", "trail", "vale", "valley", "wave", "willow", "wind", "wing", "wood", "wren",
];

const VERBS: &[&str] = &[
    "arcing",
    "blazing",
    "bowing",
    "braiding",
    "calling",
    "carving",
    "chasing",
    "climbing",
    "coiling",
    "crossing",
    "curving",
    "dancing",
    "dashing",
    "dipping",
    "diving",
    "drifting",
    "ebbing",
    "facing",
    "fading",
    "falling",
    "flowing",
    "folding",
    "forging",
    "forming",
    "gliding",
    "growing",
    "guiding",
    "holding",
    "humming",
    "jumping",
    "keeping",
    "landing",
    "leading",
    "leaning",
    "leaping",
    "lifting",
    "looking",
    "looping",
    "marking",
    "mending",
    "nesting",
    "opening",
    "pacing",
    "parting",
    "passing",
    "paving",
    "peeling",
    "planting",
    "pouring",
    "pressing",
    "pulling",
    "pushing",
    "raking",
    "reaching",
    "reading",
    "resting",
    "riding",
    "ringing",
    "rising",
    "rolling",
    "roaming",
    "rowing",
    "running",
    "rushing",
    "sailing",
    "seeking",
    "setting",
    "shaping",
    "shining",
    "singing",
    "skating",
    "soaring",
    "sorting",
    "sowing",
    "sparking",
    "spinning",
    "standing",
    "starting",
    "staying",
    "stepping",
    "stirring",
    "striding",
    "surging",
    "sweeping",
    "swirling",
    "tending",
    "threading",
    "tilting",
    "tracing",
    "trading",
    "turning",
    "vaulting",
    "wading",
    "waiting",
    "walking",
    "waving",
    "weaving",
    "winding",
    "wishing",
    "yielding",
];

/// Generate a friendly three-word plan name like "bold-river-dancing".
/// Uses a hash of session_id + timestamp to pick deterministically without randomness.
fn generate_name(session_id: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    let mut hasher = DefaultHasher::new();
    session_id.hash(&mut hasher);
    now.hash(&mut hasher);
    let h = hasher.finish();

    let adj = ADJECTIVES[(h as usize) % ADJECTIVES.len()];
    let noun = NOUNS[((h >> 16) as usize) % NOUNS.len()];
    let verb = VERBS[((h >> 32) as usize) % VERBS.len()];
    format!("{adj}-{noun}-{verb}")
}

/// Directory where plan files are stored for a session.
pub fn plans_dir(session_dir: &Path) -> PathBuf {
    session_dir.join("plans")
}

/// Write a plan to disk and return the file path.
/// If the generated name already exists, appends -2, -3, etc.
pub fn save(session_dir: &Path, session_id: &str, content: &str) -> std::io::Result<PathBuf> {
    let dir = plans_dir(session_dir);
    fs::create_dir_all(&dir)?;

    let base = generate_name(session_id);
    let mut path = dir.join(format!("{base}.md"));
    let mut n = 2u32;
    while path.exists() {
        path = dir.join(format!("{base}-{n}.md"));
        n += 1;
    }

    fs::write(&path, content)?;
    Ok(path)
}

/// List all plan files for a session.
pub fn list(session_dir: &Path) -> Vec<PathBuf> {
    let dir = plans_dir(session_dir);
    let mut plans: Vec<PathBuf> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    plans.sort();
    plans
}

/// Check if a given file path is a plan file for the given session.
pub fn is_plan_file(session_dir: &Path, file_path: &str) -> bool {
    let dir = plans_dir(session_dir);
    Path::new(file_path).starts_with(&dir)
}
