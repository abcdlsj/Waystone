package dev.waystone.app.pool

import org.json.JSONArray
import java.io.File

/**
 * Persisted list of enrolled servers stored as one small JSON file in `filesDir`.
 */
class InstanceStore(private val file: File) {
    private val _instances = mutableListOf<Instance>()
    val instances: List<Instance> get() = _instances.toList()

    init {
        if (file.isFile) {
            runCatching {
                val array = JSONArray(file.readText(Charsets.UTF_8))
                for (i in 0 until array.length()) {
                    _instances.add(Instance.fromJson(array.getJSONObject(i)))
                }
            }
        }
    }

    fun upsert(instance: Instance) {
        val index = _instances.indexOfFirst { it.id == instance.id }
        if (index >= 0) {
            _instances[index] = instance.copy(label = _instances[index].label)
        } else {
            _instances.add(instance)
        }
        save()
    }

    fun remove(id: String) {
        _instances.removeAll { it.id == id }
        save()
    }

    private fun save() {
        runCatching {
            file.parentFile?.mkdirs()
            val array = JSONArray()
            _instances.forEach { array.put(it.toJson()) }
            val tmp = File(file.parentFile, "${file.name}.tmp")
            tmp.writeText(array.toString(), Charsets.UTF_8)
            if (!tmp.renameTo(file)) {
                file.writeText(array.toString(), Charsets.UTF_8)
                tmp.delete()
            }
        }
    }
}
