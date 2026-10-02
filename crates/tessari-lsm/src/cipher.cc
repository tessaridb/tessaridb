// The engine's encryption provider for an encrypted store (ADR-0108 D7).
//
// The engine encrypts every file it writes through an EncryptionProvider: it
// asks the provider for a prefix to put at the head of a new file, and for a
// cipher stream that turns bytes at a logical file offset into ciphertext and
// back. This one writes a prefix holding a fresh random 24-byte nonce per
// file, and its stream XORs the XChaCha20 keystream of (engine subkey, that
// nonce) at the offset. The keystream and the nonce come from Rust
// (`encryption.rs`); this file only adapts them to the engine's interface,
// because that interface is C++ and its C API offers no way in.
//
// Nothing here may throw into the engine: every entry point returns a Status,
// and the one function the Rust side calls catches everything.

#include <cstdint>
#include <cstring>
#include <memory>
#include <string>

#include "rocksdb/env.h"
#include "rocksdb/env_encryption.h"

extern "C" {
// XOR the keystream of `key` and `nonce` at byte `offset` into `data`.
// Answers 0, or non-zero when the offset is past what the keystream covers.
int tessari_lsm_keystream(const uint8_t* key, const uint8_t* nonce,
                          uint64_t offset, uint8_t* data, size_t length);
// Fill `out` from the system's entropy. Answers 0, or non-zero on refusal.
int tessari_lsm_entropy(uint8_t* out, size_t length);
}

namespace {

constexpr size_t kKeyBytes = 32;
constexpr size_t kNonceBytes = 24;
// One page, so every data offset the engine aligns stays aligned on disk.
constexpr size_t kPrefixBytes = 4096;
constexpr size_t kMagicBytes = 8;
const char kMagic[kMagicBytes] = {'T', 'E', 'S', 'S', 'X', 'C', '2', '1'};
// The engine's block size is only a hint for its own splitting; the stream
// takes any offset and length.
constexpr size_t kBlockBytes = 64;

// Wipe in a way the compiler may not remove.
void Wipe(uint8_t* bytes, size_t length) {
  volatile uint8_t* at = bytes;
  for (size_t i = 0; i < length; ++i) {
    at[i] = 0;
  }
}

class Stream : public rocksdb::BlockAccessCipherStream {
 public:
  Stream(const uint8_t* key, const char* nonce) {
    std::memcpy(key_, key, kKeyBytes);
    std::memcpy(nonce_, nonce, kNonceBytes);
  }
  ~Stream() override { Wipe(key_, kKeyBytes); }

  size_t BlockSize() override { return kBlockBytes; }

  rocksdb::Status Encrypt(uint64_t offset, char* data, size_t size) override {
    return Apply(offset, data, size);
  }
  rocksdb::Status Decrypt(uint64_t offset, char* data, size_t size) override {
    return Apply(offset, data, size);
  }

 protected:
  void AllocateScratch(std::string&) override {}
  rocksdb::Status EncryptBlock(uint64_t index, char* data, char*) override {
    return Apply(index * kBlockBytes, data, kBlockBytes);
  }
  rocksdb::Status DecryptBlock(uint64_t index, char* data, char*) override {
    return Apply(index * kBlockBytes, data, kBlockBytes);
  }

 private:
  rocksdb::Status Apply(uint64_t offset, char* data, size_t size) {
    if (tessari_lsm_keystream(key_, nonce_, offset,
                              reinterpret_cast<uint8_t*>(data), size) != 0) {
      return rocksdb::Status::IOError(
          "tessaridb: an encrypted file cannot grow past 256 GiB");
    }
    return rocksdb::Status::OK();
  }

  uint8_t key_[kKeyBytes];
  uint8_t nonce_[kNonceBytes];
};

class Provider : public rocksdb::EncryptionProvider {
 public:
  explicit Provider(const uint8_t* key) { std::memcpy(key_, key, kKeyBytes); }
  ~Provider() override { Wipe(key_, kKeyBytes); }

  const char* Name() const override { return "TessariXChaCha20"; }

  size_t GetPrefixLength() const override { return kPrefixBytes; }

  rocksdb::Status CreateNewPrefix(const std::string&, char* prefix,
                                  size_t length) const override {
    if (length != kPrefixBytes) {
      return rocksdb::Status::InvalidArgument(
          "tessaridb: the engine asked for a prefix of another length");
    }
    std::memset(prefix, 0, length);
    std::memcpy(prefix, kMagic, kMagicBytes);
    if (tessari_lsm_entropy(reinterpret_cast<uint8_t*>(prefix) + kMagicBytes,
                            kNonceBytes) != 0) {
      return rocksdb::Status::IOError(
          "tessaridb: the system refused entropy for a file's nonce");
    }
    return rocksdb::Status::OK();
  }

  rocksdb::Status AddCipher(const std::string&, const char*, size_t,
                            bool) override {
    return rocksdb::Status::NotSupported(
        "tessaridb: the store's key is given when it is opened");
  }

  rocksdb::Status CreateCipherStream(
      const std::string& name, const rocksdb::EnvOptions&,
      rocksdb::Slice& prefix,
      std::unique_ptr<rocksdb::BlockAccessCipherStream>* result) override {
    if (prefix.size() < kMagicBytes + kNonceBytes ||
        std::memcmp(prefix.data(), kMagic, kMagicBytes) != 0) {
      return rocksdb::Status::Corruption(
          "tessaridb: " + name + " was not written by an encrypted store");
    }
    result->reset(new Stream(key_, prefix.data() + kMagicBytes));
    return rocksdb::Status::OK();
  }

 private:
  uint8_t key_[kKeyBytes];
};

}  // namespace

// The C API's environment handle. Its definition lives in the engine's c.cc
// and is not exported, so it is repeated here token for token: the binding's
// `Env::from_raw` takes one, and `rocksdb_env_destroy` frees it, deleting the
// environment because `is_default` is false.
struct rocksdb_env_t {
  rocksdb::Env* rep;
  bool is_default;
};

// An environment that encrypts every file under the 32-byte `key`, or null
// when it could not be made. The key is copied; the caller may wipe its own.
extern "C" rocksdb_env_t* tessari_lsm_encrypted_env(const uint8_t* key) {
  try {
    auto provider = std::make_shared<Provider>(key);
    rocksdb::Env* env = rocksdb::NewEncryptedEnv(rocksdb::Env::Default(), provider);
    if (env == nullptr) {
      return nullptr;
    }
    return new rocksdb_env_t{env, false};
  } catch (...) {
    return nullptr;
  }
}
