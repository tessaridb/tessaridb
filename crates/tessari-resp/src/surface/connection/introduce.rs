//! A connection introducing itself: `AUTH`, `HELLO` and `CLIENT`.

use tessaridb::BUILD_VERSION;
use zeroize::Zeroize as _;

use super::Connection;
use crate::codec::{Protocol, Reply};
use crate::surface::running::Failed;

impl Connection {
    /// Sign in as `user` (`default` when only a password is given).
    async fn sign_in(&mut self, user: Option<&[u8]>, password: &[u8]) -> Result<(), Reply> {
        let user = user.map_or_else(
            || "default".to_owned(),
            |user| String::from_utf8_lossy(user).into_owned(),
        );
        match self.held.sign_in(&self.shared, &user, password).await {
            Ok(()) => {
                self.user = Some(user);
                Ok(())
            }
            Err(Failed::Busy) => Err(Reply::error("TRYAGAIN the node is busy; try again")),
            Err(Failed::Lost) => Err(Reply::error("ERR the sign-in was lost; reconnect")),
            Err(Failed::Refused(_)) => Err(Reply::error(
                "WRONGPASS invalid username-password pair or user is disabled.",
            )),
        }
    }

    pub(super) async fn auth(&mut self, user: Option<&[u8]>, password: &[u8]) -> Reply {
        match self.sign_in(user, password).await {
            Ok(()) => Reply::OK,
            Err(refused) => refused,
        }
    }

    pub(super) async fn hello(
        &mut self,
        protocol: Option<i64>,
        auth: Option<(Vec<u8>, Vec<u8>)>,
        name: Option<Vec<u8>>,
    ) -> Reply {
        let chosen = match protocol {
            None => self.protocol,
            Some(2) => Protocol::Two,
            Some(3) => Protocol::Three,
            Some(_) => return Reply::error("NOPROTO unsupported protocol version"),
        };
        if let Some((user, mut password)) = auth {
            let signed = self.sign_in(Some(&user), &password).await;
            password.zeroize();
            if let Err(refused) = signed {
                return refused;
            }
        }
        if let Some(name) = name {
            if let Err(refused) = check_name(&name) {
                return refused;
            }
            self.name = Some(name);
        }
        self.protocol = chosen;
        let version = match chosen {
            Protocol::Two => 2,
            Protocol::Three => 3,
        };
        Reply::Map(vec![
            (Reply::text("server"), Reply::text("tessaridb")),
            (Reply::text("version"), Reply::text(BUILD_VERSION)),
            (Reply::text("proto"), Reply::Integer(version)),
            (
                Reply::text("id"),
                Reply::Integer(i64::try_from(self.id).unwrap_or(i64::MAX)),
            ),
            (Reply::text("mode"), Reply::text("standalone")),
            (Reply::text("role"), Reply::text("master")),
            (Reply::text("modules"), Reply::Array(Vec::new())),
        ])
    }

    pub(super) fn client(&mut self, arguments: &[Vec<u8>]) -> Reply {
        let subcommand = arguments
            .first()
            .map(|word| word.to_ascii_uppercase())
            .unwrap_or_default();
        match (subcommand.as_slice(), &arguments[1..]) {
            (b"SETNAME", [name]) => match check_name(name) {
                Ok(()) => {
                    self.name = (!name.is_empty()).then(|| name.clone());
                    Reply::OK
                }
                Err(refused) => refused,
            },
            (b"GETNAME", []) => self.name.clone().map_or(Reply::Nil, Reply::Bulk),
            (b"ID", []) => Reply::Integer(i64::try_from(self.id).unwrap_or(i64::MAX)),
            (b"SETINFO", [_, _]) | (b"NO-EVICT", [_]) | (b"NO-TOUCH", [_]) => Reply::OK,
            (b"INFO", []) | (b"LIST", _) => Reply::Bulk(self.described().into_bytes()),
            _ => Reply::error(format!(
                "ERR unknown subcommand '{}'. Try CLIENT HELP.",
                String::from_utf8_lossy(&subcommand).to_ascii_lowercase()
            )),
        }
    }

    fn described(&self) -> String {
        format!(
            "id={} addr={} name={} db=0 user={} resp={}\n",
            self.id,
            self.peer.map(|peer| peer.to_string()).unwrap_or_default(),
            self.name
                .as_deref()
                .map(String::from_utf8_lossy)
                .unwrap_or_default(),
            self.user.as_deref().unwrap_or(""),
            match self.protocol {
                Protocol::Two => 2,
                Protocol::Three => 3,
            }
        )
    }
}

/// A client name may not hold a space or anything unprintable.
fn check_name(name: &[u8]) -> Result<(), Reply> {
    if name.iter().all(|byte| byte.is_ascii_graphic()) {
        Ok(())
    } else {
        Err(Reply::error(
            "ERR Client names cannot contain spaces, newlines or special characters.",
        ))
    }
}
