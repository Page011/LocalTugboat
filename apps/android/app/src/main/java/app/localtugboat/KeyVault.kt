package app.localtugboat

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.io.File
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec
import uniffi.tugboat_ffi.generateSecretKey

/** The identity key could not be read. The app cannot sync until the user resets it: never silently replaced. */
class KeyVaultException(message: String, cause: Throwable? = null) : Exception(message, cause)

/** File layout of the wrapped key: one byte IV length, the IV, then the AES-GCM ciphertext (key + 16 byte tag). */
object WrapFormat {
    fun encode(iv: ByteArray, ct: ByteArray): ByteArray {
        require(iv.size in 1..255) { "bad iv length ${iv.size}" }
        return byteArrayOf(iv.size.toByte()) + iv + ct
    }

    /** Returns (iv, ciphertext). Throws IllegalArgumentException for a truncated or empty file. */
    fun decode(blob: ByteArray): Pair<ByteArray, ByteArray> {
        require(blob.isNotEmpty()) { "empty" }
        val n = blob[0].toInt() and 0xFF
        require(n > 0 && blob.size > 1 + n) { "truncated" }
        return blob.copyOfRange(1, 1 + n) to blob.copyOfRange(1 + n, blob.size)
    }
}

/**
 * The iroh secret key, wrapped with a non-exportable Android Keystore AES-GCM key. Only the wrapped
 * bytes touch disk (filesDir/identity.key); allowBackup=false and the extraction rules keep that
 * file out of cloud backup and device transfer, so a copy on another phone would be unreadable anyway.
 */
object KeyVault {
    const val ALIAS = "tugboat_identity_wrap"

    /**
     * 0.1.0 wrapped identity.key under this alias (and used it as the GCM associated data). A Keystore key cannot
     * be renamed or exported, so an updated phone can only unwrap its old identity with it. Never created here,
     * never deleted: it is used only when it already exists in the Keystore.
     */
    const val LEGACY_ALIAS = "snapferry_identity_wrap"
    private const val FILE = "identity.key"
    private const val TAG_BITS = 128

    private fun file(c: Context) = File(c.applicationContext.filesDir, FILE)

    fun exists(c: Context) = file(c).exists()

    /** Unwrap the key, or create and store one on the very first run. Blocking (Keystore + disk). */
    @Synchronized
    fun loadOrCreate(c: Context): ByteArray {
        val f = file(c)
        if (!f.exists()) {
            val key = generateSecretKey()
            store(f, key)
            KLog.log(c, "identity key created")
            return key
        }
        try {
            val (iv, ct) = WrapFormat.decode(f.readBytes())
            val ks = keyStore()
            val order = AliasOrder.of(ks.aliases().toList().toSet())
            val (alias, key) = AliasOrder.firstThatWorks(order) { a ->
                val cipher = Cipher.getInstance("AES/GCM/NoPadding")
                cipher.init(Cipher.DECRYPT_MODE, ks.getKey(a, null) as SecretKey, GCMParameterSpec(TAG_BITS, iv))
                cipher.updateAAD(a.toByteArray())
                cipher.doFinal(ct)
            }
            if (alias == LEGACY_ALIAS) KLog.log(c, "identity key unwrapped with the 0.1.0 alias")
            return key
        } catch (e: Exception) {
            // Keystore wiped (lock screen reset on some devices, restored app data) or file damaged.
            // Making a new key would be a new identity the PC does not know, so stop and say so.
            throw KeyVaultException("KEY UNREADABLE - Tugboat cannot unlock its identity. Clear app data, then pair again.", e)
        }
    }

    private fun store(f: File, key: ByteArray) {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, wrapKey())
        cipher.updateAAD(ALIAS.toByteArray())
        val blob = WrapFormat.encode(cipher.iv, cipher.doFinal(key))
        // Write then rename: a crash mid-write must not leave a half file that reads as "unreadable key".
        val tmp = File(f.parentFile, "$FILE.tmp")
        tmp.writeBytes(blob)
        if (!tmp.renameTo(f)) throw KeyVaultException("cannot store the identity key at ${f.path}")
    }

    private fun keyStore(): KeyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }

    private fun wrapKey(): SecretKey {
        val ks = keyStore()
        (ks.getKey(ALIAS, null) as? SecretKey)?.let { return it }
        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        gen.init(
            KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build()
        )
        return gen.generateKey()
    }
}

/** Which Keystore aliases may unwrap the identity key, in the order tried. Pure, so the JVM test covers it. */
object AliasOrder {
    /** The current alias first, then the 0.1.0 one; only those that exist in the Keystore. */
    fun of(present: Set<String>): List<String> =
        listOf(KeyVault.ALIAS, KeyVault.LEGACY_ALIAS).filter { it in present }

    /** First alias whose attempt does not throw, with its result. Throws the last failure (or IllegalState when none exist). */
    fun <T> firstThatWorks(order: List<String>, attempt: (String) -> T): Pair<String, T> {
        var last: Exception? = null
        for (a in order) {
            try {
                return a to attempt(a)
            } catch (e: Exception) {
                last = e
            }
        }
        throw last ?: IllegalStateException("no identity wrap key in the Keystore")
    }
}
