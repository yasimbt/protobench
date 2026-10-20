//! Project configuration (`project.toml`): model, loading, validation and
//! migration.

mod load;
mod model;
mod validate;

pub use load::{
    deserialize_tracking, load_config_file, load_config_str, locate_line, migrate, path_to_string,
    LoadedConfig, Seg,
};
pub use model::*;
pub use validate::{parse_colour, parse_id_key, parse_sim_action, validate, Finding, SimAction};
