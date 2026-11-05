package app.rustsync

import android.app.Activity
import android.content.Context
import android.content.ContextWrapper
import android.content.pm.PackageManager
import android.nfc.NfcAdapter
import android.nfc.Tag
import android.nfc.cardemulation.HostApduService
import android.nfc.tech.IsoDep
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import java.io.ByteArrayOutputStream

/** Whether this phone can use NFC at all, and whether it is switched on. */
enum class NfcState { Missing, Off, Ready }

/**
 * Handing a pairing code to another phone by holding the two together.
 *
 * The phone showing the code acts like a contactless card that answers only to RustSync's own
 * application id; the other phone reads it. The code is the same text as in the QR code, and
 * pairing still ends with both people comparing the verification code, so NFC is only another way
 * to carry the code across.
 *
 * The exchange (ISO 7816 style, so Android's NFC stack routes it to us):
 *   reader -> SELECT <our application id>      card -> [length of the code, 2 bytes] 90 00
 *   reader -> READ <offset> <how much>         card -> that part of the code 90 00   (repeated)
 */
object NfcPairing {
    /** An application id only RustSync answers to: F0 marks a private one, then the letters "RUSTSYNC". */
    val AID = byteArrayOf(0xF0.toByte(), 0x52, 0x55, 0x53, 0x54, 0x53, 0x59, 0x4E, 0x43)

    private const val MAX_CODE = 2000
    private const val CHUNK = 200

    /** The code to give to a phone that asks, or null when none is being shown. */
    @Volatile
    private var offered: ByteArray? = null

    /** Start (or, with null, stop) handing out this code. Only set while the code is on screen. */
    fun offer(code: String?) {
        offered = code?.toByteArray(Charsets.UTF_8)?.takeIf { it.size in 1..MAX_CODE }
    }

    internal fun offeredCode(): ByteArray? = offered

    /** Can this phone act as the card (share a code with another phone)? */
    fun canOffer(context: Context): Boolean =
        context.packageManager.hasSystemFeature(PackageManager.FEATURE_NFC_HOST_CARD_EMULATION)

    fun state(context: Context): NfcState {
        val adapter = NfcAdapter.getDefaultAdapter(context) ?: return NfcState.Missing
        return if (adapter.isEnabled) NfcState.Ready else NfcState.Off
    }

    /** Read a code from the next phone that is held against this one, while [activity] is shown. */
    fun startReading(activity: Activity, onCode: (String) -> Unit, onProblem: (String) -> Unit) {
        val adapter = NfcAdapter.getDefaultAdapter(activity) ?: return
        val main = Handler(Looper.getMainLooper())
        adapter.enableReaderMode(
            activity,
            NfcAdapter.ReaderCallback { tag ->
                try {
                    val code = readCode(tag)
                    main.post { onCode(code) }
                } catch (e: Exception) {
                    main.post { onProblem(e.message ?: "could not read the other phone") }
                }
            },
            NfcAdapter.FLAG_READER_NFC_A or NfcAdapter.FLAG_READER_NFC_B or NfcAdapter.FLAG_READER_SKIP_NDEF_CHECK,
            null,
        )
    }

    fun stopReading(activity: Activity) {
        try {
            NfcAdapter.getDefaultAdapter(activity)?.disableReaderMode(activity)
        } catch (_: Exception) {
        }
    }

    private fun ok(response: ByteArray): Boolean =
        response.size >= 2 && response[response.size - 2] == 0x90.toByte() && response[response.size - 1] == 0x00.toByte()

    private fun readCode(tag: Tag): String {
        val iso = IsoDep.get(tag) ?: throw IllegalStateException("that is not a phone showing a RustSync code")
        iso.connect()
        iso.timeout = 5000
        try {
            val select = byteArrayOf(0x00, 0xA4.toByte(), 0x04, 0x00, AID.size.toByte()) + AID + byteArrayOf(0x00)
            val first = iso.transceive(select)
            if (!ok(first) || first.size != 4) throw IllegalStateException("the other phone is not showing a pairing code")
            val total = ((first[0].toInt() and 0xFF) shl 8) or (first[1].toInt() and 0xFF)
            if (total < 1 || total > MAX_CODE) throw IllegalStateException("the other phone sent something unexpected")
            val out = ByteArrayOutputStream()
            while (out.size() < total) {
                val offset = out.size()
                val want = minOf(CHUNK, total - offset)
                val read = byteArrayOf(0x00, 0xB0.toByte(), (offset shr 8).toByte(), (offset and 0xFF).toByte(), want.toByte())
                val part = iso.transceive(read)
                if (!ok(part) || part.size <= 2) throw IllegalStateException("the code did not come across; hold the phones still")
                out.write(part, 0, part.size - 2)
            }
            val text = String(out.toByteArray(), Charsets.UTF_8).trim()
            if (!text.contains('@') || !text.contains('.')) throw IllegalStateException("that is not a RustSync pairing code")
            return text
        } finally {
            try {
                iso.close()
            } catch (_: Exception) {
            }
        }
    }
}

/** The activity a composable's context belongs to (it may be wrapped). */
fun Context.findActivity(): Activity? {
    var c: Context? = this
    while (c is ContextWrapper) {
        if (c is Activity) return c
        c = c.baseContext
    }
    return null
}

/**
 * The "card" side: answers a phone that holds itself against this one, but only while a pairing
 * code is on screen (see [NfcPairing.offer]); otherwise it says it has nothing.
 */
class PairingCardService : HostApduService() {
    override fun processCommandApdu(apdu: ByteArray?, extras: Bundle?): ByteArray {
        val a = apdu ?: return SW_NOT_SUPPORTED
        if (a.size < 5 || a[0] != 0x00.toByte()) return SW_NOT_SUPPORTED
        val code = NfcPairing.offeredCode() ?: return SW_NOT_FOUND
        return when (a[1]) {
            0xA4.toByte() -> {
                val lc = a[4].toInt() and 0xFF
                if (a.size < 5 + lc || !a.copyOfRange(5, 5 + lc).contentEquals(NfcPairing.AID)) {
                    SW_NOT_FOUND
                } else {
                    byteArrayOf((code.size shr 8).toByte(), (code.size and 0xFF).toByte()) + SW_OK
                }
            }
            0xB0.toByte() -> {
                val offset = ((a[2].toInt() and 0xFF) shl 8) or (a[3].toInt() and 0xFF)
                val le = (a[4].toInt() and 0xFF).let { if (it == 0) 256 else it }
                if (offset >= code.size) {
                    SW_WRONG_PARAMETERS
                } else {
                    code.copyOfRange(offset, minOf(code.size, offset + le)) + SW_OK
                }
            }
            else -> SW_NOT_SUPPORTED
        }
    }

    override fun onDeactivated(reason: Int) {}

    private companion object {
        val SW_OK = byteArrayOf(0x90.toByte(), 0x00)
        val SW_NOT_FOUND = byteArrayOf(0x6A, 0x82.toByte())
        val SW_WRONG_PARAMETERS = byteArrayOf(0x6B, 0x00)
        val SW_NOT_SUPPORTED = byteArrayOf(0x6D, 0x00)
    }
}
