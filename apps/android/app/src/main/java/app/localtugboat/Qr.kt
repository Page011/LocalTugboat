package app.localtugboat

import androidx.camera.core.ExperimentalGetImage
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import com.google.mlkit.vision.barcode.BarcodeScannerOptions
import com.google.mlkit.vision.barcode.BarcodeScanning
import com.google.mlkit.vision.barcode.common.Barcode
import com.google.mlkit.vision.common.InputImage

/**
 * CameraX analyzer for the "Scan the QR code on your PC" screen (bundled ML Kit model, no Play Services).
 * Calls [onPayload] for QR codes that look like a Tugboat pairing payload; every other QR code in
 * view is ignored without a word. The real validation is inspectPairingPayload in the core.
 * Call [close] when the screen goes away.
 */
class QrAnalyzer(private val onPayload: (String) -> Unit, private val onError: (String) -> Unit) : ImageAnalysis.Analyzer {
    private val scanner = BarcodeScanning.getClient(
        BarcodeScannerOptions.Builder().setBarcodeFormats(Barcode.FORMAT_QR_CODE).build()
    )

    @ExperimentalGetImage
    override fun analyze(imageProxy: ImageProxy) {
        val media = imageProxy.image
        if (media == null) {
            imageProxy.close()
            return
        }
        scanner.process(InputImage.fromMediaImage(media, imageProxy.imageInfo.rotationDegrees))
            .addOnSuccessListener { codes -> codes.firstNotNullOfOrNull { PairingText.candidate(it.rawValue) }?.let(onPayload) }
            .addOnFailureListener { onError("QR scan failed: ${it.message}") }
            .addOnCompleteListener { imageProxy.close() }
    }

    fun close() = scanner.close()
}
