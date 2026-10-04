pub const USAGE: &str = "\
usage: tessaridb [<path> | --at <host:port>] [-e <script> | -f <file>]

  <path>          a store on disk; omitted, the store is in memory and is lost
  --at <host:port> a running node, instead of a store in this process
  --user <name>   sign in as this user; the password comes from TESSARIDB_PASSWORD,
                  never from an argument, which the process table would publish
  --serve <host:port> serve this store over the wire protocol until stopped
  --http <host:port> serve this store over HTTP until stopped; may accompany
                  --serve, and one process then holds both
  --backup-dir <folder> where a serving node writes `BACKUP … TO '<name>'`;
                  without it every `TO` is refused
  --unseal-for <duration> how long an unseal lasts before the store seals itself,
                  written as TessariQL (`10m`, `1h`); default TESSARIDB_UNSEAL_FOR
                  or 10m
  --tls-cert <file> serve clients over TLS with this certificate chain, PEM;
                  default TESSARIDB_TLS_CERT
  --tls-key <file> its private key, PEM; default TESSARIDB_TLS_KEY
  --require-client-tls refuse to start unless --tls-cert and --tls-key are
                  given, for a deployment that forbids the clear; or
                  TESSARIDB_REQUIRE_CLIENT_TLS=1; off by default
  --client-plaintext refused since 0.24.0-beta: the clear is the default without
                  a certificate, so the flag is dropped rather than given
  --tls-authority <file> with --at: speak TLS and trust the node by these
                  certificates, PEM; default TESSARIDB_TLS_AUTHORITY
  --cluster-credential <file> this node's peer credential, PEM, with --serve
  --cluster-key <file> its private key, PEM
  --cluster-authority <file> the one certificate this cluster trusts, PEM
  --cluster-address <host:port> where this node's own peer door binds
  --seed <node-id>@<host:port> a node to reach the cluster through; repeatable
  --join-token <token> a token from CREATE JOIN TOKEN, offered to the seeds until
                  a row names this node; default TESSARIDB_JOIN_TOKEN
  --encryption-key-file <file> the store's files and every backup it writes are
                  encrypted under the 32 bytes in <file> (`openssl rand 32`,
                  mode 600); a store opens only the way it was created; default
                  TESSARIDB_ENCRYPTION_KEY_FILE
  --backup-key-file <file> with --restore or --verify: the key a sealed backup
                  was sealed under, when it is not the store's — how a store
                  moves to a new key
  --param <name>=<value> bind $name to <value>, written as TessariQL; repeatable
  -e, --execute <script> run this and exit
  -f, --file <file> run this file and exit
  --backup <file> write the store's current state (a snapshot) to <file> and
                  exit; with --from, its log instead
  --snapshot <file> write the store's current state to <file> and exit
  --dump <file>   write the store's current state as TessariQL to <file> and exit
  --verify <file> read <file> and say what it holds, changing nothing
  --from <n>      with --backup: write the log from <n> (1 for the whole log)
  --upto <n>      with --restore: stop replaying after sequence <n>
  --restore <file> replay <file> into an empty store and exit
  --health        say whether the store is well, and exit non-zero if not
  -V, --version   say which build this is, and exit
  -h, --help      this

a node given --tls-cert and --tls-key speaks TLS on every client surface — the
wire port, HTTP and the WebSocket on it — and nothing else. Without one a node,
single or clustered, serves its clients in the clear and says so when it starts;
--require-client-tls makes it refuse to start instead. The peer link is mutual
TLS whatever the clients were given.

the five cluster options are given together or not at all: told some of them a
node refuses to start rather than serving with credentials nobody checked, and
told none of them it is the single node it is today. there is no default peer
address, because there is no port this engine claims and a defaulted listener is
a door the operator did not know they opened.

with neither -e nor -f, statements are read from standard input: a prompt when
that is a terminal, a script when it is a pipe.

serving a store that has no users yet, TESSARIDB_INITIAL_USER and
TESSARIDB_INITIAL_PASSWORD declare that user as a store-wide owner and close the
store. Both or neither: half of them is refused rather than started, because a
node that came up open because a variable was misspelled looks exactly like one
that came up correctly. A store that already has users ignores them, so a
container may carry them on every restart, and they are not a way to reset a
password.

a serving node keeps the newest 100000 records of each log and prunes the rest;
TESSARIDB_RETAIN_RECORDS sets another count, or `none` for every record, and
`DEFINE NODE RETAIN` stored on the node wins over both.";
