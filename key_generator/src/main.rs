pub mod depositor_key;
pub mod exitor_key;
pub mod transactor_key;
pub mod read_write_keys;
pub mod vk_contract_generator;

use depositor_key::make_depositor_key;
use exitor_key::make_exitor_key;
use transactor_key::make_transactor_key;

fn main() {
    make_depositor_key();
    make_exitor_key();
    make_transactor_key();
}
