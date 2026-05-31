local str = "Jelly Think"
result = 0
if string.len(str) ~= 11 then result = -1 end
str = "ab"
if string.rep(str, 2) ~= "abab" then result = -1 end
str = "Jelly Think"
if string.lower(str) ~= "jelly think" then result = -1 end
if string.upper(str) == "JELLY THINK" then result = -1 end
return result
