use crate::{
    contact,
    db,
    models::*,
    monero,
    reqres,
    utils,
};
use log::{
    debug,
    error,
    info,
};
use rocket::serde::json::Json;

/*
  TODOs(c2m):
    - API to validate payment and import multisig info, update to multisig complete
    - API to upload gpg encrypted tracking number, update order to shipped
    - release tracking (locker code?) when txset is released, update to delivered
*/

enum StatusType {
    _Delivered,
    MultisigMissing,
    _MulitsigComplete,
    _Shipped,
}

impl StatusType {
    pub fn value(&self) -> String {
        match *self {
            StatusType::_Delivered => String::from("Delivered"),
            StatusType::MultisigMissing => String::from("MultisigMissing"),
            StatusType::_MulitsigComplete => String::from("MulitsigComplete"),
            StatusType::_Shipped => String::from("Shipped"),
        }
    }
}

/// Create a intial order
pub async fn create(j_order: Json<reqres::OrderRequest>) -> Order {
    info!("creating order");
    let wallet_name = String::from(crate::APP_NAME);
    let wallet_password =
        std::env::var(crate::MONERO_WALLET_PASSWORD).unwrap_or(String::from("password"));
    monero::close_wallet(&wallet_name, &wallet_password).await;
    let ts = chrono::offset::Utc::now().timestamp();
    let orid: String = format!("O{}", utils::generate_rnd());
    let r_subaddress = monero::create_address().await;
    let subaddress = r_subaddress.result.address;
    let new_order = Order {
        orid: String::from(&orid),
        cid: String::from(&j_order.cid),
        pid: String::from(&j_order.pid),
        date: ts,
        ship_address: j_order.ship_address.iter().cloned().collect(),
        subaddress,
        status: StatusType::MultisigMissing.value(),
        quantity: j_order.quantity,
        ..Default::default()
    };
    debug!("insert order: {:?}", new_order);
    let m_wallet = monero::create_wallet(&orid, &utils::empty_string()).await;
    if !m_wallet {
        error!("error creating msig wallet for order {}", &orid);
        monero::close_wallet(&orid, &wallet_password).await;
        return Default::default();
    }
    debug!("insert order: {:?}", &new_order);
    let s = db::Interface::open();
    let k = &new_order.orid;
    db::Interface::write(&s.env, &s.handle, k, &Order::to_db(&new_order));
    // in order to retrieve all orders, write keys to with ol
    let list_key = format!("ol");
    let r = db::Interface::read(&s.env, &s.handle, &String::from(&list_key));
    if r == utils::empty_string() {
        debug!("creating order index");
    }
    let order_list = [r, String::from(&orid)].join(",");
    debug!("writing order index {} for id: {}", order_list, list_key);
    db::Interface::write(&s.env, &s.handle, &String::from(list_key), &order_list);
    monero::close_wallet(&orid, &wallet_password).await;
    new_order
}

/// Lookup order
pub fn find(oid: &String) -> Order {
    info!("find order: {}", &oid);
    let s = db::Interface::open();
    let r = db::Interface::read(&s.env, &s.handle, &String::from(oid));
    if r == utils::empty_string() {
        error!("order not found");
        return Default::default();
    }
    Order::from_db(String::from(oid), r)
}

/// Lookup all orders from admin server
pub fn find_all() -> Vec<Order> {
    let i_s = db::Interface::open();
    let i_list_key = format!("ol");
    let i_r = db::Interface::read(&i_s.env, &i_s.handle, &String::from(i_list_key));
    if i_r == utils::empty_string() {
        error!("order index not found");
    }
    let i_v_oid = i_r.split(",");
    let i_v: Vec<String> = i_v_oid.map(|s| String::from(s)).collect();
    let mut orders: Vec<Order> = Vec::new();
    for o in i_v {
        let order: Order = find(&o);
        if order.orid != utils::empty_string() {
            orders.push(order);
        }
    }
    orders
}

/// Lookup all orders for customer
pub async fn find_all_customer_orders(cid: String) -> Vec<Order> {
    info!("lookup orders for customer: {}", &cid);
    let i_s = db::Interface::open();
    let i_list_key = format!("ol");
    let i_r = db::Interface::read(&i_s.env, &i_s.handle, &String::from(i_list_key));
    if i_r == utils::empty_string() {
        error!("order index not found");
    }
    let i_v_oid = i_r.split(",");
    let i_v: Vec<String> = i_v_oid.map(|s| String::from(s)).collect();
    let mut orders: Vec<Order> = Vec::new();
    for o in i_v {
        let order: Order = find(&o);
        if order.orid != utils::empty_string() && order.cid == cid {
            orders.push(order);
        }
    }
    orders
}

/// Modify order from admin server
pub fn modify(o: Json<Order>) -> Order {
    info!("modify order: {}", &o.orid);
    let f_order: Order = find(&o.orid);
    if f_order.orid == utils::empty_string() {
        error!("order not found");
        return Default::default();
    }
    let u_order = Order::update(String::from(&f_order.orid), &o);
    let s = db::Interface::open();
    db::Interface::delete(&s.env, &s.handle, &u_order.pid);
    db::Interface::write(&s.env, &s.handle, &u_order.pid, &Order::to_db(&u_order));
    return u_order;
}

/// Sign and submit multisig
pub async fn sign_and_submit_multisig(
    orid: &String,
    tx_data_hex: &String,
) -> reqres::XmrRpcSubmitMultisigResponse {
    info!("signing and submitting multisig");
    let r_sign: reqres::XmrRpcSignMultisigResponse =
        monero::sign_multisig(String::from(tx_data_hex)).await;
    let r_submit: reqres::XmrRpcSubmitMultisigResponse =
        monero::submit_multisig(r_sign.result.tx_data_hex).await;
    if r_submit.result.tx_hash_list.is_empty() {
        error!("unable to submit payment for order: {}", orid);
    }
    r_submit
}

/// In order for the order (...ha) to only be accessed by the customer
///
/// they must sign the order id with their NEVEKO wallet instance. This means
///
/// that the mediator can see order id for disputes without being able to access
///
/// the details of said order.
pub async fn retrieve_order(orid: &String, signature: &String) -> Order {
    // get customer address for NEVEKO NOT order wallet
    let m_order: Order = find(&orid);
    let mut xmr_address: String = String::new();
    let a_customers: Vec<Contact> = contact::find_all();
    for customer in a_customers {
        if customer.i2p_address == m_order.cid {
            xmr_address = customer.xmr_address;
        }
    }
    // send address, orid and signature to verify()
    let id: String = String::from(&m_order.orid);
    let sig: String = String::from(signature);
    let is_valid_signature = monero::verify(xmr_address, id, sig).await;
    if !is_valid_signature {
        return Default::default();
    }
    m_order
}

pub async fn validate_order_for_ship() -> bool {
    info!("validating order for shipment");
    // import multisig info

    // check balance and unlock_time

    // update the order status to multisig complete
    return false;
}
