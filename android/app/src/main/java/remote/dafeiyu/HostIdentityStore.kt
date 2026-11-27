package remote.dafeiyu

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.io.File
import java.io.FileOutputStream
import java.nio.channels.FileChannel
import java.nio.file.StandardOpenOption
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.spec.ECGenParameterSpec
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

object HostIdentityStore {
    private const val KEYSTORE = "AndroidKeyStore"
    private const val WRAPPING_KEY = "dfy.android.host-identity.wrapping.v1"
    private const val FILE_NAME = "host_identity.pkcs8.enc"
    private const val VERSION = 1
    private const val IV_BYTES = 12
    private const val TAG_BITS = 128
    private const val MAX_FILE_BYTES = 16 * 1024

    @Synchronized
    fun loadOrCreate(context: Context): ByteArray {
        val file = File(context.applicationContext.filesDir, FILE_NAME)
        val directory = requireNotNull(file.parentFile)
        if (file.exists()) {
            val stored = file.readBytes()
            require(stored.size in VERSION + IV_BYTES + TAG_BITS / 8 + 1..MAX_FILE_BYTES) {
                "Android 设备身份文件损坏"
            }
            require(stored[0].toInt() == VERSION) { "Android 设备身份版本不受支持" }
            val cipher = Cipher.getInstance("AES/GCM/NoPadding")
            cipher.init(
                Cipher.DECRYPT_MODE,
                wrappingKey(),
                GCMParameterSpec(TAG_BITS, stored, 1, IV_BYTES),
            )
            return cipher.doFinal(stored, 1 + IV_BYTES, stored.size - 1 - IV_BYTES)
                .also { require(it.size in 64..4096) { "Android 设备身份私钥无效" } }
        }
        val generator = KeyPairGenerator.getInstance("EC")
        generator.initialize(ECGenParameterSpec("secp256r1"))
        val key = generator.generateKeyPair().private.encoded
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, wrappingKey())
        val iv = cipher.iv
        val encrypted = cipher.doFinal(key)
        val payload = byteArrayOf(VERSION.toByte()) + iv + encrypted
        val temporary = File(directory, "$FILE_NAME.tmp")
        FileOutputStream(temporary).use { output ->
            output.write(payload)
            output.fd.sync()
        }
        check(temporary.renameTo(file)) { "无法持久化 Android 设备身份" }
        FileChannel.open(directory.toPath(), StandardOpenOption.READ).use { channel ->
            channel.force(true)
        }
        return key
    }

    private fun wrappingKey(): SecretKey {
        val keyStore = KeyStore.getInstance(KEYSTORE).apply { load(null) }
        (keyStore.getKey(WRAPPING_KEY, null) as? SecretKey)?.let { return it }
        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE)
        generator.init(
            KeyGenParameterSpec.Builder(
                WRAPPING_KEY,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build(),
        )
        return generator.generateKey()
    }
}
