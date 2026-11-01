package app.rustsync

import com.journeyapps.barcodescanner.CaptureActivity

/**
 * The QR scanner screen. The library's own one is fixed to landscape, so the phone had to be
 * turned sideways; this one only differs in the manifest, where it is allowed to be upright.
 */
class PortraitCaptureActivity : CaptureActivity()
