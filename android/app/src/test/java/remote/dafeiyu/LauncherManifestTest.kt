package remote.dafeiyu

import java.io.File
import javax.xml.parsers.DocumentBuilderFactory
import org.junit.Assert.assertEquals
import org.junit.Test
import org.w3c.dom.Element

class LauncherManifestTest {
    @Test
    fun onlyMainActivityIsALauncher() {
        val workingDirectory = requireNotNull(System.getProperty("user.dir"))
        val manifest = generateSequence(File(workingDirectory).absoluteFile) { it.parentFile }
            .map { File(it, "app/src/main/AndroidManifest.xml") }
            .firstOrNull { it.isFile }
            ?: error("AndroidManifest.xml not found")
        val factory = DocumentBuilderFactory.newInstance().apply {
            isNamespaceAware = true
        }
        val document = factory.newDocumentBuilder().parse(manifest)
        val application = document.getElementsByTagName("application").item(0) as Element
        val launchers = application.children()
            .filter { it.tagName == "activity" || it.tagName == "activity-alias" }
            .filter { component ->
                component.children().any { filter ->
                    filter.tagName == "intent-filter" &&
                        filter.children().any { it.tagName == "action" && it.androidName == "android.intent.action.MAIN" } &&
                        filter.children().any { it.tagName == "category" && it.androidName == "android.intent.category.LAUNCHER" }
                }
            }
            .map { it.androidName }

        assertEquals(listOf(".MainActivity"), launchers)
    }

    private fun Element.children(): List<Element> =
        (0 until childNodes.length).mapNotNull { childNodes.item(it) as? Element }

    private val Element.androidName: String
        get() = getAttributeNS("http://schemas.android.com/apk/res/android", "name")
}
